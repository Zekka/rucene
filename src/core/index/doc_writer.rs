// Copyright 2019 Zhizhesihai (Beijing) Technology Limited.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use core::codec::Codec;
use core::index::doc_writer_delete_queue::DocumentsWriterDeleteQueue;
use core::index::doc_writer_flush_queue::DocumentsWriterFlushQueue;
use core::index::flush_control::DocumentsWriterFlushControl;
use core::index::flush_policy::FlushByRamOrCountsPolicy;
use core::index::index_writer::{IndexWriter, IndexWriterInner};
use core::index::index_writer_config::IndexWriterConfig;
use core::index::thread_doc_writer::{
    DocumentsWriterPerThread, DocumentsWriterPerThreadPool, ThreadState,
};
use core::index::{Fieldable, SegmentInfo, Term};
use core::search::Query;
use core::store::{Directory, LockValidatingDirectoryWrapper};
use core::util::Volatile;
use error::{ErrorKind::AlreadyClosed, Result};

use crossbeam::queue::SegQueue;

use core::index::merge_policy::MergePolicy;
use core::index::merge_scheduler::MergeScheduler;
use std::cmp::max;
use std::collections::HashSet;
use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;

///
// This class accepts multiple added documents and directly
// writes segment files.
//
// Each added document is passed to the indexing chain,
// which in turn processes the document into the different
// codec formats.  Some formats write bytes to files
// immediately, e.g. stored fields and term vectors, while
// others are buffered by the indexing chain and written
// only on flush.
//
// Once we have used our allowed RAM buffer, or the number
// of added docs is large enough (in the case we are
// flushing by doc count instead of RAM usage), we create a
// real segment and flush it to the Directory.
//
// Threads:
//
// Multiple threads are allowed into addDocument at once.
// There is an initial synchronized call to getThreadState
// which allocates a ThreadState for this thread.  The same
// thread will get the same ThreadState over time (thread
// affinity) so that if there are consistent patterns (for
// example each thread is indexing a different content
// source) then we make better use of RAM.  Then
// processDocument is called on that ThreadState without
// synchronization (most of the "heavy lifting" is in this
// call).  Finally the synchronized "finishDocument" is
// called to flush changes to the directory.
//
// When flush is called by IndexWriter we forcefully idle
// all threads and flush only once they are all idle.  This
// means you can call flush with a given thread even while
// other threads are actively adding/deleting documents.
//
//
// Exceptions:
//
// Because this class directly updates in-memory posting
// lists, and flushes stored fields and term vectors
// directly to files in the directory, there are certain
// limited times when an exception can corrupt this state.
// For example, a disk full while flushing stored fields
// leaves this file in a corrupt state.  Or, an OOM
// exception while appending to the in-memory posting lists
// can corrupt that posting list.  We call such exceptions
// "aborting exceptions".  In these cases we must call
// abort() to discard all docs added since the last flush.
//
// All other exceptions ("non-aborting exceptions") can
// still partially update the index structures.  These
// updates are consistent, but, they represent only a part
// of the document seen up until the exception was hit.
// When this happens, we immediately mark the document as
// deleted so that the document is always atomically ("all
// or none") added to the index.
//
pub(crate) struct DocumentsWriter<
    D: Directory + Send + Sync + 'static,
    C: Codec,
    MS: MergeScheduler,
    MP: MergePolicy,
> {
    lock: Arc<Mutex<()>>,
    directory_orig: Arc<D>,
    directory: Arc<LockValidatingDirectoryWrapper<D>>,
    closed: bool,
    num_docs_in_ram: AtomicU32,
    // TODO: cut over to BytesRefHash in BufferedDeletes
    pub delete_queue: Arc<DocumentsWriterDeleteQueue<C>>,
    ticket_queue: DocumentsWriterFlushQueue<D, C>,

    // we preserve changes during a full flush since IW might not checkout
    // before we release all changes. NTR Readers otherwise suddenly return
    // true from is_current while there are actually changes currently
    // committed. See also self.any_change() & self.flush_all_threads.
    pending_changes_in_current_full_flush: Volatile<bool>,
    pub per_thread_pool: DocumentsWriterPerThreadPool<D, C, MS, MP>,
    pub flush_policy: Arc<FlushByRamOrCountsPolicy<C, MS, MP>>,
    flush_control: DocumentsWriterFlushControl<D, C, MS, MP>,
    config: Arc<IndexWriterConfig<C, MS, MP>>,
    writer: Weak<IndexWriterInner<D, C, MS, MP>>,
    pub events: SegQueue<WriterEvent<D, C>>,
    pub last_seq_no: u64,
    inited: bool,
    // must init flush_control after new
}

impl<D, C, MS, MP> DocumentsWriter<D, C, MS, MP>
where
    D: Directory + Send + Sync + 'static,
    C: Codec,
    MS: MergeScheduler,
    MP: MergePolicy,
{
    pub fn new(
        config: Arc<IndexWriterConfig<C, MS, MP>>,
        directory_orig: Arc<D>,
        directory: Arc<LockValidatingDirectoryWrapper<D>>,
    ) -> Self {
        let flush_policy = Arc::new(FlushByRamOrCountsPolicy::new(Arc::clone(&config)));
        let flush_control =
            DocumentsWriterFlushControl::new(Arc::clone(&config), Arc::clone(&flush_policy));
        DocumentsWriter {
            lock: Arc::new(Mutex::new(())),
            directory_orig,
            directory,
            closed: false,
            num_docs_in_ram: AtomicU32::new(0),
            delete_queue: Arc::new(DocumentsWriterDeleteQueue::default()),
            ticket_queue: DocumentsWriterFlushQueue::new(),
            pending_changes_in_current_full_flush: Volatile::new(false),
            per_thread_pool: DocumentsWriterPerThreadPool::new(),
            flush_policy,
            flush_control,
            config,
            writer: Weak::new(),
            events: SegQueue::new(),
            last_seq_no: 0,
            inited: false,
        }
    }

    pub(crate) fn init(&mut self, writer: Weak<IndexWriterInner<D, C, MS, MP>>) {
        self.writer = writer;
        unsafe {
            let flush_control =
                &mut self.flush_control as *mut DocumentsWriterFlushControl<D, C, MS, MP>;
            (*flush_control).init(
                self,
                self.writer.upgrade().unwrap().buffered_updates_stream(),
            );
        }
        self.inited = true;
    }

    fn writer(&self) -> Arc<IndexWriterInner<D, C, MS, MP>> {
        debug_assert!(self.inited);
        self.writer.upgrade().unwrap()
    }

    unsafe fn doc_writer_mut(&self, _l: &MutexGuard<()>) -> &mut DocumentsWriter<D, C, MS, MP> {
        let w = self as *const DocumentsWriter<D, C, MS, MP> as *mut DocumentsWriter<D, C, MS, MP>;
        &mut *w
    }

    pub(crate) fn set_delete_queue(&self, delete_queue: Arc<DocumentsWriterDeleteQueue<C>>) {
        // let l = self.lock.lock().unwrap();
        // let doc_writer_mut = unsafe { self.doc_writer_mut(&l) };

        // already locked
        let w = self as *const DocumentsWriter<D, C, MS, MP> as *mut DocumentsWriter<D, C, MS, MP>;
        unsafe {
            (*w).delete_queue = delete_queue;
        }
    }

    pub fn update_documents<F: Fieldable>(
        &self,
        docs: Vec<Vec<F>>,
        // analyzer: Analyzer,
        del_term: Option<Term>,
    ) -> Result<(u64, bool)> {
        debug_assert!(self.inited);
        let has_events = self.pre_update()?;

        let per_thread = self.flush_control.obtain_and_lock()?;
        let (seq_no, flush_dwpt) = {
            let l = match per_thread.lock.try_lock() {
                Ok(g) => g,
                Err(e) => {
                    bail!(
                        "update document try obtain per_thread.lock failed by: {:?}",
                        e
                    );
                }
            };
            let per_thread_mut = per_thread.thread_state_mut(&l);
            self.do_update_documents(per_thread_mut, docs, del_term)?
        };

        self.per_thread_pool.release(per_thread);

        let has_event = self.post_update(flush_dwpt, has_events)?;
        Ok((seq_no, has_event))
    }

    fn do_update_documents<F: Fieldable>(
        &self,
        per_thread: &mut ThreadState<D, C, MS, MP>,
        docs: Vec<Vec<F>>,
        // analyzer: Analyzer,
        del_term: Option<Term>,
    ) -> Result<(u64, Option<DocumentsWriterPerThread<D, C, MS, MP>>)> {
        let is_update = del_term.is_some();

        // This must happen after we've pulled the ThreadState because IW.close
        // waits for all ThreadStates to be released:
        self.ensure_open()?;
        self.ensure_inited(per_thread)?;
        debug_assert!(per_thread.inited());
        let dwpt_num_docs = per_thread.dwpt().num_docs_in_ram;

        let res = per_thread.dwpt_mut().update_documents(docs, del_term);
        let num_docs_in_ram = if res.is_err() {
            // TODO, we should only deal with AbortException here instead of
            // all errors
            let mut dwpt = self.flush_control.do_on_abort(per_thread);
            dwpt.as_mut().unwrap().abort();
            dwpt.as_ref().unwrap().num_docs_in_ram
        } else {
            per_thread.dwpt().num_docs_in_ram
        };

        // We don't know whether the document actually
        // counted as being indexed, so we must subtract here to
        // accumulate our separate counter:
        self.num_docs_in_ram
            .fetch_add(num_docs_in_ram - dwpt_num_docs, Ordering::AcqRel);

        let seq_no = match res {
            Ok(n) => n,
            Err(e) => {
                return Err(e);
            }
        };

        let flush_dwpt = self
            .flush_control
            .do_after_document(per_thread, is_update)?;

        debug_assert!(seq_no > per_thread.last_seq_no());
        per_thread.set_last_seq_no(seq_no);
        Ok((seq_no, flush_dwpt))
    }

    pub fn update_document<F: Fieldable>(
        &self,
        doc: Vec<F>,
        // analyzer: Analyzer,
        del_term: Option<Term>,
    ) -> Result<(u64, bool)> {
        debug_assert!(self.inited);
        let mut has_event = self.pre_update()?;

        let per_thread = self.flush_control.obtain_and_lock()?;
        let (seq_no, flush_dwpt) = {
            let guard = match per_thread.lock.try_lock() {
                Ok(g) => g,
                Err(e) => {
                    bail!(
                        "update document try obtain per_thread.state failed by: {:?}",
                        e
                    );
                }
            };
            let per_thread_mut = per_thread.thread_state_mut(&guard);
            self.do_update_document(per_thread_mut, doc, del_term)?
        };
        self.per_thread_pool.release(per_thread);

        has_event = self.post_update(flush_dwpt, has_event)?;

        Ok((seq_no, has_event))
    }

    fn do_update_document<F: Fieldable>(
        &self,
        per_thread: &mut ThreadState<D, C, MS, MP>,
        doc: Vec<F>,
        // analyzer: Analyzer,
        del_term: Option<Term>,
    ) -> Result<(u64, Option<DocumentsWriterPerThread<D, C, MS, MP>>)> {
        let is_update = del_term.is_some();

        // This must happen after we've pulled the ThreadState because IW.close
        // waits for all ThreadStates to be released:
        self.ensure_open()?;
        self.ensure_inited(per_thread)?;
        debug_assert!(per_thread.inited());

        let dwpt_num_docs = per_thread.dwpt().num_docs_in_ram;
        let res = per_thread.dwpt_mut().update_document(doc, del_term);
        let num_docs_in_ram = if res.is_err() {
            // TODO, we should only deal with AbortException here instead of
            // all errors
            let mut dwpt = self.flush_control.do_on_abort(per_thread);

            dwpt.as_mut().unwrap().abort();
            dwpt.as_ref().unwrap().num_docs_in_ram
        } else {
            per_thread.dwpt().num_docs_in_ram
        };

        // We don't know whether the document actually
        // counted as being indexed, so we must subtract here to
        // accumulate our separate counter:
        self.num_docs_in_ram
            .fetch_add(num_docs_in_ram - dwpt_num_docs, Ordering::AcqRel);

        let seq_no = match res {
            Ok(n) => n,
            Err(e) => {
                return Err(e);
            }
        };

        let flush_dwpt = self
            .flush_control
            .do_after_document(per_thread, is_update)?;

        debug_assert!(seq_no > per_thread.last_seq_no());
        per_thread.set_last_seq_no(seq_no);
        Ok((seq_no, flush_dwpt))
    }

    fn pre_update(&self) -> Result<bool> {
        self.ensure_open()?;
        let mut has_events = false;
        if self.flush_control.any_stalled_threads() || self.flush_control.num_queued_flushes() > 0 {
            // Help out flushing any queued DWPTs so we can un-stall:
            loop {
                // Try pick up pending threads here if possible
                loop {
                    if let Some(dwpt) = self.flush_control.next_pending_flush() {
                        // Don't push the delete here since the update could fail!
                        has_events |= self.do_flush(dwpt)?;
                    } else {
                        break;
                    }
                }
                self.flush_control.wait_if_stalled()?; // block is stalled
                if self.flush_control.num_queued_flushes() == 0 {
                    break;
                }
            }
        }
        Ok(has_events)
    }

    fn post_update(
        &self,
        flushing_dwpt: Option<DocumentsWriterPerThread<D, C, MS, MP>>,
        mut has_events: bool,
    ) -> Result<bool> {
        has_events |= self.apply_all_deletes_local()?;
        if let Some(dwpt) = flushing_dwpt {
            has_events |= self.do_flush(dwpt)?;
        } else {
            if let Some(mut next_pending_flush) = self.flush_control.next_pending_flush() {
                has_events |= self.do_flush(next_pending_flush)?;
            }
        }

        Ok(has_events)
    }

    fn ensure_inited(&self, state: &mut ThreadState<D, C, MS, MP>) -> Result<()> {
        let writer = self.writer();
        if state.dwpt.is_none() {
            let segment_name = writer.new_segment_name();
            let pending_num_docs = Arc::clone(&writer.pending_num_docs);
            let dwpt = DocumentsWriterPerThread::new(
                self.writer.clone(),
                segment_name,
                Arc::clone(&self.directory_orig),
                Arc::clone(&self.directory),
                Arc::clone(&self.config),
                Arc::clone(&self.delete_queue),
                pending_num_docs,
            )?;

            state.dwpt = Some(dwpt);
            let codec = Arc::clone(&writer.config.codec);
            state
                .dwpt
                .as_mut()
                .unwrap()
                .init(Arc::clone(&writer.global_field_numbers()), codec);
        }
        Ok(())
    }

    pub fn delete_queries(&self, queries: Vec<Arc<dyn Query<C>>>) -> Result<(u64, bool)> {
        debug_assert!(self.inited);
        // TODO why is this synchronized?
        let l = self.lock.lock()?;
        let doc_writer_mut = unsafe { self.doc_writer_mut(&l) };
        let seq_no = self.delete_queue.add_delete_queries(queries)?;
        doc_writer_mut.flush_control.do_on_delete();

        let applyed = self.apply_all_deletes_local()?;
        doc_writer_mut.last_seq_no = max(self.last_seq_no, seq_no);
        Ok((seq_no, applyed))
    }

    pub fn delete_terms(&self, terms: Vec<Term>) -> Result<(u64, bool)> {
        debug_assert!(self.inited);
        // TODO why is this synchronized?
        let l = self.lock.lock()?;
        let doc_writer_mut = unsafe { self.doc_writer_mut(&l) };
        let seq_no = self.delete_queue.add_delete_terms(terms)?;
        doc_writer_mut.flush_control.do_on_delete();

        let applyed = self.apply_all_deletes_local()?;
        doc_writer_mut.last_seq_no = max(self.last_seq_no, seq_no);
        Ok((seq_no, applyed))
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs_in_ram.load(Ordering::Acquire)
    }

    fn apply_all_deletes_local(&self) -> Result<bool> {
        if self.flush_control.get_and_reset_apply_all_deletes() {
            if !self.flush_control.is_full_flush() {
                self.ticket_queue.add_deletes(&self.delete_queue)?;
            }
            self.put_event(WriterEvent::ApplyDeletes);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn put_event(&self, event: WriterEvent<D, C>) {
        self.events.push(event);
    }

    pub fn purge_buffer(&self, writer: &IndexWriter<D, C, MS, MP>, forced: bool) -> Result<u32> {
        if forced {
            self.ticket_queue.force_purge(writer)
        } else {
            self.ticket_queue.try_purge(writer)
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            bail!(AlreadyClosed("this IndexWriter is closed".into()));
        }
        Ok(())
    }

    /// Called if we hit an exception at a bad time (when
    /// updating the index files) and must discard all
    /// currently buffered docs.  This resets our state,
    /// discarding any docs added since last flush.
    pub fn abort(&mut self) -> Result<()> {
        let lock = Arc::clone(&self.lock);
        let _l = lock.lock()?;
        self.delete_queue.clear()?;
        debug!("DW: start to abort");

        for i in 0..self.per_thread_pool.active_thread_state_count() {
            let per_thread = Arc::clone(&self.per_thread_pool.get_thread_state(i));
            let lock_guard = per_thread.lock.lock()?;
            let per_thread_mut = per_thread.thread_state_mut(&lock_guard);
            self.abort_thread_state(per_thread_mut);
        }
        self.flush_control.abort_pending_flushes();
        self.flush_control.wait_for_flush()?;
        debug!("DW: done abort succeeded!");
        Ok(())
    }

    /// Return how many documents were aborted
    /// _l is IndexWriter.full_flush_lock guard
    pub fn lock_and_abort_all(&self, _l: &MutexGuard<()>) -> Result<u32> {
        debug!("DW - lock_and_abort_all");

        let mut aborted_doc_count = 0;
        self.delete_queue.clear()?;
        self.per_thread_pool.set_abort();
        for i in 0..self.per_thread_pool.active_thread_state_count() {
            let per_thread = Arc::clone(&self.per_thread_pool.get_thread_state(i));
            let guard = per_thread.lock.lock().unwrap();
            let per_thread_mut = per_thread.thread_state_mut(&guard);
            aborted_doc_count += self.abort_thread_state(per_thread_mut);
        }
        self.delete_queue.clear()?;

        // jump over any possible in flight ops:
        let jump = self.per_thread_pool.active_thread_state_count() + 1;
        self.delete_queue.skip_sequence_number(jump as u64);

        self.flush_control.abort_pending_flushes();
        self.flush_control.wait_for_flush()?;
        Ok(aborted_doc_count)
    }

    /// Returns how many documents were aborted.
    fn abort_thread_state(&self, per_thread: &mut ThreadState<D, C, MS, MP>) -> u32 {
        if per_thread.inited() {
            let aborted_doc_count = per_thread.dwpt().num_docs_in_ram;
            self.subtract_flushed_num_docs(aborted_doc_count);
            per_thread.dwpt_mut().abort();
            self.flush_control.do_on_abort(per_thread);
            aborted_doc_count
        } else {
            self.flush_control.do_on_abort(per_thread);
            0
        }
    }

    fn do_flush(&self, mut dwpt: DocumentsWriterPerThread<D, C, MS, MP>) -> Result<bool> {
        let mut has_events = false;
        loop {
            let res = self.flush_dwpt(&mut dwpt, &mut has_events);
            if res.is_ok() {
                // Now we are done and try to flush the ticket queue if the head of the
                // queue has already finished the flush.
                if self.ticket_queue.ticket_count() as usize
                    >= self.per_thread_pool.active_thread_state_count()
                {
                    // This means there is a backlog: the one
                    // thread in innerPurge can't keep up with all
                    // other threads flushing segments.  In this case
                    // we forcefully stall the producers.
                    self.put_event(WriterEvent::ForcedPurge);
                    self.flush_control.after_flush(dwpt);
                    break;
                }
            }
            self.flush_control.after_flush(dwpt);
            res?;

            match self.flush_control.next_pending_flush() {
                Some(writer) => {
                    dwpt = writer;
                }
                None => {
                    break;
                }
            }
        }
        if has_events {
            self.put_event(WriterEvent::MergePending);
        }

        // If deletes alone are consuming > 1/2 our RAM
        // buffer, force them all to apply now. This is to
        // prevent too-frequent flushing of a long tail of
        // tiny segments:
        if self.config.flush_on_ram()
            && self.flush_control.delete_bytes_used() > self.config.ram_buffer_size() / 2
        {
            has_events = true;
            if !self.apply_all_deletes_local()? {
                debug!("DW: force apply deletes");
                self.put_event(WriterEvent::ApplyDeletes);
            }
        }

        Ok(has_events)
    }

    fn flush_dwpt(
        &self,
        dwpt: &mut DocumentsWriterPerThread<D, C, MS, MP>,
        has_events: &mut bool,
    ) -> Result<()> {
        // Since with DWPT the flush process is concurrent and several DWPT
        // could flush at the same time we must maintain the order of the
        // flushes before we can apply the flushed segment and the frozen global
        // deletes it is buffering. The reason for this is that the global
        // deletes mark a certain point in time where we took a DWPT out of
        // rotation and freeze the global deletes.
        //
        // Example: A flush 'A' starts and freezes the global deletes, then
        // flush 'B' starts and freezes all deletes occurred since 'A' has
        // started. if 'B' finishes before 'A' we need to wait until 'A' is done
        // otherwise the deletes frozen by 'B' are not applied to 'A' and we
        // might miss to deletes documents in 'A'.

        // Each flush is assigned a ticket in the order they acquire the
        // ticket_queue lock
        let res = {
            let ticket = self.ticket_queue.add_flush_ticket(dwpt)?;

            match dwpt.flush() {
                Ok(seg) => {
                    unsafe {
                        (*ticket).set_segment(seg);
                    }
                    Ok(())
                }
                Err(e) => {
                    error!("dwpt flush failed by {:?}", e);
                    // In the case of a failure make sure we are making progress and
                    // apply all the deletes since the segment flush failed since the flush
                    // ticket could hold global deletes see FlushTicket#canPublish()
                    unsafe {
                        (*ticket).set_failed();
                    }
                    Err(e)
                }
            }
        };
        let flushing_docs_in_ram = dwpt.num_docs_in_ram;
        self.subtract_flushed_num_docs(flushing_docs_in_ram);
        if !dwpt.files_to_delete.is_empty() {
            let files_to_delete = mem::replace(&mut dwpt.files_to_delete, HashSet::new());
            self.put_event(WriterEvent::DeleteNewFiles(files_to_delete));
            *has_events = true;
        }
        if res.is_err() {
            self.put_event(WriterEvent::FlushFailed(dwpt.segment_info.clone()));
            *has_events = true;
        }
        res
    }

    pub fn any_changes(&self) -> bool {
        // changes are either in a DWPT or in the deleteQueue.
        // yet if we currently flush deletes and / or dwpt there
        // could be a window where all changes are in the ticket queue
        // before they are published to the IW. ie we need to check if the
        // ticket queue has any tickets.
        self.num_docs_in_ram.load(Ordering::Acquire) > 0
            || self.delete_queue.any_changes()
            || self.ticket_queue.has_tickets()
            || self.pending_changes_in_current_full_flush.read()
    }

    pub fn subtract_flushed_num_docs(&self, num_flushed: u32) {
        debug_assert!(self.num_docs_in_ram.load(Ordering::Acquire) >= num_flushed);
        self.num_docs_in_ram
            .fetch_sub(num_flushed, Ordering::AcqRel);
    }

    /// FlushAllThreads is synced by IW fullFlushLock. Flushing all threads is a
    /// two stage operation; the caller must ensure (in try/finally) that finishFlush
    /// is called after this method, to release the flush lock in DWFlushControl
    pub fn flush_all_threads(&self) -> Result<(bool, u64)> {
        debug!("DW: start full flush");

        let (seq_no, flushing_queue) = {
            let _l = self.lock.lock()?;
            self.pending_changes_in_current_full_flush
                .write(self.any_changes());
            self.flush_control.mark_for_full_flush()
        };

        let mut anything_flushed = false;
        loop {
            // Help out with flushing:
            if let Some(flushing_dwpt) = self.flush_control.next_pending_flush() {
                anything_flushed |= self.do_flush(flushing_dwpt)?;
            } else {
                break;
            }
        }
        // If a concurrent flush is still in flight wait for it
        self.flush_control.wait_for_flush()?;
        if !anything_flushed && flushing_queue.any_changes() {
            // apply deletes if we did not flush any document
            debug!(
                "DW - {:?}: flush naked frozen global deletes",
                thread::current().name()
            );

            self.ticket_queue.add_deletes(flushing_queue.as_ref())?;
        }
        let index_writer = IndexWriter::with_inner(self.writer());
        self.ticket_queue.force_purge(&index_writer)?;
        debug_assert!(!flushing_queue.any_changes() && !self.ticket_queue.has_tickets());

        Ok((anything_flushed, seq_no))
    }

    pub fn finish_full_flush(&self, success: bool) {
        debug!(
            "DW - {:?} finish full flush, success={}",
            thread::current().name(),
            success
        );
        if success {
            self.flush_control.finish_full_flush();
        } else {
            self.flush_control.abort_full_flushes();
        }
        self.pending_changes_in_current_full_flush.write(false);
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.flush_control.set_closed();
    }
}

impl<D, C, MS, MP> Drop for DocumentsWriter<D, C, MS, MP>
where
    D: Directory + Send + Sync + 'static,
    C: Codec,
    MS: MergeScheduler,
    MP: MergePolicy,
{
    fn drop(&mut self) {
        self.closed = true;
        self.flush_control.set_closed();
    }
}

/// Interface for internal atomic events. See {@link DocumentsWriter} for details.
/// Events are executed concurrently and no order is guaranteed. Each event should
/// only rely on the serializeability within its process method. All actions that
/// must happen before or after a certain action must be encoded inside the
/// {@link #process(IndexWriter, boolean, boolean)} method.
pub trait Event<D: Directory + Send + Sync + 'static, C: Codec> {
    /// Processes the event. This method is called by the `IndexWriter`
    /// passed as the first argument.
    fn process<MS: MergeScheduler, MP: MergePolicy>(
        &self,
        writer: &IndexWriter<D, C, MS, MP>,
        trigger_merge: bool,
        clear_buffer: bool,
    ) -> Result<()>;
}

pub enum WriterEvent<D: Directory, C: Codec> {
    ApplyDeletes,
    MergePending,
    ForcedPurge,
    FlushFailed(SegmentInfo<D, C>),
    DeleteNewFiles(HashSet<String>),
}

impl<D: Directory + Send + Sync + 'static, C: Codec> Event<D, C> for WriterEvent<D, C> {
    fn process<MS: MergeScheduler, MP: MergePolicy>(
        &self,
        writer: &IndexWriter<D, C, MS, MP>,
        trigger_merge: bool,
        clear_buffer: bool,
    ) -> Result<()> {
        match self {
            WriterEvent::ApplyDeletes => writer.apply_deletes_and_purge(true),
            WriterEvent::MergePending => {
                writer.do_after_segment_flushed(trigger_merge, clear_buffer)
            }
            WriterEvent::ForcedPurge => {
                writer.purge(true)?;
                Ok(())
            }
            WriterEvent::FlushFailed(s) => writer.flush_failed(s),
            WriterEvent::DeleteNewFiles(files) => writer.delete_new_files(files),
        }
    }
}
