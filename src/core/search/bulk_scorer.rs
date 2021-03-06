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

use core::search::collector::Collector;
use core::search::{Scorer, NO_MORE_DOCS};
use core::util::Bits;
use core::util::DocId;
use error::Result;

pub struct BulkScorer<'a, S: Scorer + ?Sized + 'a> {
    pub scorer: &'a mut S,
}

impl<'a, S: Scorer + ?Sized + 'a> BulkScorer<'a, S> {
    pub fn new(scorer: &'a mut S) -> BulkScorer<'a, S> {
        BulkScorer { scorer }
    }

    /// Collects matching documents in a range and return an estimation of the
    /// next matching document which is on or after `max`.
    ///
    /// *Arguments*
    ///     * `min` Score starting at, including, this document.
    ///     * `max` Score up to, but not including, this doc.
    ///
    /// The return value must be:
    ///     * >= `max`
    ///     * `DocIterator::NO_MORE_DOCS` if there are no more matches
    ///     * <= the first matching document that is >= `max` otherwise.
    ///
    /// `min` is the minimum document to be considered for matching. All
    /// documents strictly before this value must be ignored.
    ///
    /// Although `max` would be a legal return value for this method, higher
    /// values might help callers skip more efficiently over non-matching portions
    /// of the docID space.
    pub fn score<T: Collector + ?Sized, B: Bits + ?Sized>(
        &mut self,
        collector: &mut T,
        accept_docs: Option<&B>,
        min: DocId,
        max: DocId,
    ) -> Result<DocId> {
        let current_doc = if min == 0 && max == NO_MORE_DOCS {
            self.scorer.approximate_next()?
        } else {
            self.scorer.approximate_advance(min)?
        };

        self.score_range(collector, accept_docs, current_doc, max)
    }

    fn score_range<T: Collector + ?Sized, B: Bits + ?Sized>(
        &mut self,
        collector: &mut T,
        accept_docs: Option<&B>,
        min: DocId,
        max: DocId,
    ) -> Result<DocId> {
        if let Some(bits) = accept_docs {
            self.score_range_in_docs_set(collector, bits, min, max)
        } else {
            self.score_range_all(collector, min, max)
        }
    }

    fn score_range_in_docs_set<T: Collector + ?Sized, B: Bits + ?Sized>(
        &mut self,
        collector: &mut T,
        accept_docs: &B,
        min: DocId,
        max: DocId,
    ) -> Result<DocId> {
        let mut current_doc = min;
        if self.scorer.support_two_phase() {
            while current_doc < max {
                if accept_docs.get(current_doc as usize)? && self.scorer.matches()? {
                    collector.collect(current_doc, self.scorer)?;
                }
                current_doc = self.scorer.approximate_next()?;
            }
        } else {
            while current_doc < max {
                if accept_docs.get(current_doc as usize)? {
                    collector.collect(current_doc, self.scorer)?;
                }
                current_doc = self.scorer.next()?;
            }
        }
        Ok(current_doc)
    }

    fn score_range_all<T: Collector + ?Sized>(
        &mut self,
        collector: &mut T,
        min: DocId,
        max: DocId,
    ) -> Result<DocId> {
        let mut current_doc = min;
        if self.scorer.support_two_phase() {
            while current_doc < max {
                if self.scorer.matches()? {
                    collector.collect(current_doc, self.scorer)?;
                }
                current_doc = self.scorer.approximate_next()?;
            }
        } else {
            while current_doc < max {
                collector.collect(current_doc, self.scorer)?;
                current_doc = self.scorer.next()?;
            }
        }
        Ok(current_doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::search::tests::*;

    use core::index::tests::*;
    use core::index::IndexReader;
    use core::search::collector::top_docs::*;
    use core::search::collector::SearchCollector;
    use core::util::*;

    #[test]
    fn test_score() {
        let docs = vec![1, 2, 3, 4, 5];
        let bits = MatchAllBits::new(docs.len());
        let mut scorer_box = create_mock_scorer(docs);
        let leaf_reader = MockLeafReader::new(0);
        let index_reader = MockIndexReader::new(vec![leaf_reader]);
        let leaf_reader_context = index_reader.leaves();
        let mut top_collector = TopDocsCollector::new(3);
        {
            let mut bulk_scorer = BulkScorer::new(&mut scorer_box);
            top_collector
                .set_next_reader(&leaf_reader_context[0])
                .unwrap();
            bulk_scorer
                .score(&mut top_collector, Some(&bits), 0, NO_MORE_DOCS)
                .unwrap();
        }

        let top_docs = top_collector.top_docs();
        assert_eq!(top_docs.total_hits(), 5);

        let score_docs = top_docs.score_docs();
        assert_eq!(score_docs.len(), 3);
        assert_eq!(score_docs[0].doc_id(), 5);
        assert_eq!(score_docs[1].doc_id(), 4);
        assert_eq!(score_docs[2].doc_id(), 3);
    }
}
