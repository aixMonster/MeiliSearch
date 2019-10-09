use std::time::{Instant, Duration};
use std::ops::Range;
use std::mem;

use fst::{IntoStreamer, Streamer};
use sdset::SetBuf;
use slice_group_by::{GroupBy, GroupByMut};

use crate::automaton::{Automaton, AutomatonProducer, QueryEnhancer};
use crate::raw_document::{RawDocument, raw_documents_from};
use crate::{Document, DocumentId, Highlight, TmpMatch, criterion::Criteria};
use crate::{store, MResult, reordered_attrs::ReorderedAttrs};

pub struct QueryBuilder<'a> {
    criteria: Criteria<'a>,
    searchables_attrs: Option<ReorderedAttrs>,
    timeout: Duration,
    main_store: store::Main,
    postings_lists_store: store::PostingsLists,
    synonyms_store: store::Synonyms,
}

fn multiword_rewrite_matches(
    mut matches: Vec<(DocumentId, TmpMatch)>,
    query_enhancer: &QueryEnhancer,
) -> SetBuf<(DocumentId, TmpMatch)>
{
    let mut padded_matches = Vec::with_capacity(matches.len());

    // we sort the matches by word index to make them rewritable
    matches.sort_unstable_by_key(|(id, match_)| (*id, match_.attribute, match_.word_index));

    // for each attribute of each document
    for same_document_attribute in matches.linear_group_by_key(|(id, m)| (*id, m.attribute)) {

        // padding will only be applied
        // to word indices in the same attribute
        let mut padding = 0;
        let mut iter = same_document_attribute.linear_group_by_key(|(_, m)| m.word_index);

        // for each match at the same position
        // in this document attribute
        while let Some(same_word_index) = iter.next() {

            // find the biggest padding
            let mut biggest = 0;
            for (id, match_) in same_word_index {

                let mut replacement = query_enhancer.replacement(match_.query_index);
                let replacement_len = replacement.len();
                let nexts = iter.remainder().linear_group_by_key(|(_, m)| m.word_index);

                if let Some(query_index) = replacement.next() {
                    let word_index = match_.word_index + padding as u16;
                    let match_ = TmpMatch { query_index, word_index, ..match_.clone() };
                    padded_matches.push((*id, match_));
                }

                let mut found = false;

                // look ahead and if there already is a match
                // corresponding to this padding word, abort the padding
                'padding: for (x, next_group) in nexts.enumerate() {

                    for (i, query_index) in replacement.clone().enumerate().skip(x) {
                        let word_index = match_.word_index + padding as u16 + (i + 1) as u16;
                        let padmatch = TmpMatch { query_index, word_index, ..match_.clone() };

                        for (_, nmatch_) in next_group {
                            let mut rep = query_enhancer.replacement(nmatch_.query_index);
                            let query_index = rep.next().unwrap();
                            if query_index == padmatch.query_index {

                                if !found {
                                    // if we find a corresponding padding for the
                                    // first time we must push preceding paddings
                                    for (i, query_index) in replacement.clone().enumerate().take(i) {
                                        let word_index = match_.word_index + padding as u16 + (i + 1) as u16;
                                        let match_ = TmpMatch { query_index, word_index, ..match_.clone() };
                                        padded_matches.push((*id, match_));
                                        biggest = biggest.max(i + 1);
                                    }
                                }

                                padded_matches.push((*id, padmatch));
                                found = true;
                                continue 'padding;
                            }
                        }
                    }

                    // if we do not find a corresponding padding in the
                    // next groups so stop here and pad what was found
                    break
                }

                if !found {
                    // if no padding was found in the following matches
                    // we must insert the entire padding
                    for (i, query_index) in replacement.enumerate() {
                        let word_index = match_.word_index + padding as u16 + (i + 1) as u16;
                        let match_ = TmpMatch { query_index, word_index, ..match_.clone() };
                        padded_matches.push((*id, match_));
                    }

                    biggest = biggest.max(replacement_len - 1);
                }
            }

            padding += biggest;
        }
    }

    for document_matches in padded_matches.linear_group_by_key_mut(|(id, _)| *id) {
        document_matches.sort_unstable();
    }

    SetBuf::new_unchecked(padded_matches)
}

fn fetch_raw_documents(
    reader: &impl rkv::Readable,
    automatons: &[Automaton],
    query_enhancer: &QueryEnhancer,
    searchables: Option<&ReorderedAttrs>,
    main_store: &store::Main,
    postings_lists_store: &store::PostingsLists,
) -> MResult<Vec<RawDocument>>
{
    let mut matches = Vec::new();
    let mut highlights = Vec::new();

    for automaton in automatons {
        let Automaton { index, is_exact, query_len, .. } = automaton;
        let dfa = automaton.dfa();

        let words = match main_store.words_fst(reader)? {
            Some(words) => words,
            None => return Ok(Vec::new()),
        };

        let mut stream = words.search(&dfa).into_stream();
        while let Some(input) = stream.next() {
            let distance = dfa.eval(input).to_u8();
            let is_exact = *is_exact && distance == 0 && input.len() == *query_len;

            let doc_indexes = match postings_lists_store.postings_list(reader, input)? {
                Some(doc_indexes) => doc_indexes,
                None => continue,
            };

            matches.reserve(doc_indexes.len());
            highlights.reserve(doc_indexes.len());

            for di in doc_indexes.as_ref() {
                let attribute = searchables.map_or(Some(di.attribute), |r| r.get(di.attribute));
                if let Some(attribute) = attribute {
                    let match_ = TmpMatch {
                        query_index: *index as u32,
                        distance,
                        attribute,
                        word_index: di.word_index,
                        is_exact,
                    };

                    let highlight = Highlight {
                        attribute: di.attribute,
                        char_index: di.char_index,
                        char_length: di.char_length,
                    };

                    matches.push((di.document_id, match_));
                    highlights.push((di.document_id, highlight));
                }
            }
        }
    }

    let matches = multiword_rewrite_matches(matches, &query_enhancer);
    let highlights = {
        highlights.sort_unstable_by_key(|(id, _)| *id);
        SetBuf::new_unchecked(highlights)
    };

    Ok(raw_documents_from(matches, highlights))
}

impl<'a> QueryBuilder<'a> {
    pub fn new(
        main: store::Main,
        postings_lists: store::PostingsLists,
        synonyms: store::Synonyms,
    ) -> QueryBuilder<'a> {
        QueryBuilder {
            criteria: Criteria::default(),
            searchables_attrs: None,
            timeout: Duration::from_secs(1),
            main_store: main,
            postings_lists_store: postings_lists,
            synonyms_store: synonyms,
        }
    }

    pub fn query(
        self,
        reader: &impl rkv::Readable,
        query: &str,
        range: Range<usize>,
    ) -> MResult<Vec<Document>>
    {
        let start_processing = Instant::now();
        let mut raw_documents_processed = Vec::new();

        let (automaton_producer, query_enhancer) = AutomatonProducer::new(
                reader,
                query,
                self.main_store,
                self.synonyms_store,
            )?;

        let mut automaton_producer = automaton_producer.into_iter();
        let mut automatons = Vec::new();

        // aggregate automatons groups by groups after time
        while let Some(auts) = automaton_producer.next() {
            automatons.extend(auts);

            // we must retrieve the documents associated
            // with the current automatons
            let mut raw_documents = fetch_raw_documents(
                reader,
                &automatons,
                &query_enhancer,
                self.searchables_attrs.as_ref(),
                &self.main_store,
                &self.postings_lists_store,
            )?;

            let mut groups = vec![raw_documents.as_mut_slice()];

            'criteria: for criterion in self.criteria.as_ref() {
                let tmp_groups = mem::replace(&mut groups, Vec::new());
                let mut documents_seen = 0;

                for group in tmp_groups {
                    // if this group does not overlap with the requested range,
                    // push it without sorting and splitting it
                    if documents_seen + group.len() < range.start {
                        documents_seen += group.len();
                        groups.push(group);
                        continue;
                    }

                    group.sort_unstable_by(|a, b| criterion.evaluate(a, b));

                    for group in group.binary_group_by_mut(|a, b| criterion.eq(a, b)) {
                        documents_seen += group.len();
                        groups.push(group);

                        // we have sort enough documents if the last document sorted is after
                        // the end of the requested range, we can continue to the next criterion
                        if documents_seen >= range.end { continue 'criteria }
                    }
                }
            }

            // once we classified the documents related to the current
            // automatons we save that as the next valid result
            let iter = raw_documents.into_iter().skip(range.start).take(range.len());
            raw_documents_processed.clear();
            raw_documents_processed.extend(iter);

            // stop processing after there is no time
            if start_processing.elapsed() > self.timeout { break }
        }

        // make real documents now that we know
        // those must be returned
        let documents = raw_documents_processed
            .into_iter()
            .map(|d| Document::from_raw(d))
            .collect();

        Ok(documents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeSet, HashMap};
    use std::iter::FromIterator;

    use fst::{Set, IntoStreamer};
    use sdset::SetBuf;
    use tempfile::TempDir;

    use crate::automaton::normalize_str;
    use crate::database::{Database, BoxUpdateFn};
    use crate::DocIndex;
    use crate::store::Index;

    fn set_from_stream<'f, I, S>(stream: I) -> Set
    where
        I: for<'a> fst::IntoStreamer<'a, Into=S, Item=&'a [u8]>,
        S: 'f + for<'a> fst::Streamer<'a, Item=&'a [u8]>,
    {
        let mut builder = fst::SetBuilder::memory();
        builder.extend_stream(stream).unwrap();
        builder.into_inner().and_then(Set::from_bytes).unwrap()
    }

    fn insert_key(set: &Set, key: &[u8]) -> Set {
        let unique_key = {
            let mut builder = fst::SetBuilder::memory();
            builder.insert(key).unwrap();
            builder.into_inner().and_then(Set::from_bytes).unwrap()
        };

        let union_ = set.op().add(unique_key.into_stream()).r#union();

        set_from_stream(union_)
    }

    fn sdset_into_fstset(set: &sdset::Set<&str>) -> Set {
        let mut builder = fst::SetBuilder::memory();
        let set = SetBuf::from_dirty(set.into_iter().map(|s| normalize_str(s)).collect());
        builder.extend_iter(set.into_iter()).unwrap();
        builder.into_inner().and_then(Set::from_bytes).unwrap()
    }

    const fn doc_index(document_id: u64, word_index: u16) -> DocIndex {
        DocIndex {
            document_id: DocumentId(document_id),
            attribute: 0,
            word_index,
            char_index: 0,
            char_length: 0,
        }
    }

    const fn doc_char_index(document_id: u64, word_index: u16, char_index: u16) -> DocIndex {
        DocIndex {
            document_id: DocumentId(document_id),
            attribute: 0,
            word_index,
            char_index,
            char_length: 0,
        }
    }

    pub struct TempDatabase {
        database: Database,
        index: Index,
        _tempdir: TempDir,
    }

    impl TempDatabase {
        pub fn query_builder(&self) -> QueryBuilder {
            self.index.query_builder()
        }

        pub fn add_synonym(&mut self, word: &str, new: SetBuf<&str>) {
            let rkv = self.database.rkv.read().unwrap();
            let mut writer = rkv.write().unwrap();

            let word = word.to_lowercase();

            let alternatives = match self.index.synonyms.synonyms(&writer, word.as_bytes()).unwrap() {
                Some(alternatives) => alternatives,
                None => fst::Set::default(),
            };

            let new = sdset_into_fstset(&new);
            let new_alternatives = set_from_stream(alternatives.op().add(new.into_stream()).r#union());
            self.index.synonyms.put_synonyms(&mut writer, word.as_bytes(), &new_alternatives).unwrap();

            let synonyms = match self.index.main.synonyms_fst(&writer).unwrap() {
                Some(synonyms) => synonyms,
                None => fst::Set::default(),
            };

            let synonyms_fst = insert_key(&synonyms, word.as_bytes());
            self.index.main.put_synonyms_fst(&mut writer, &synonyms_fst).unwrap();

            writer.commit().unwrap();
        }
    }

    impl<'a> FromIterator<(&'a str, &'a [DocIndex])> for TempDatabase {
        fn from_iter<I: IntoIterator<Item=(&'a str, &'a [DocIndex])>>(iter: I) -> Self {
            let tempdir = TempDir::new().unwrap();
            let database = Database::open_or_create(&tempdir).unwrap();
            let update_fn = None as Option::<BoxUpdateFn>;
            let index = database.open_index("default", update_fn).unwrap();

            let rkv = database.rkv.read().unwrap();
            let mut writer = rkv.write().unwrap();

            let mut words_fst = BTreeSet::new();
            let mut postings_lists = HashMap::new();

            for (word, indexes) in iter {
                let word = word.to_lowercase().into_bytes();
                words_fst.insert(word.clone());
                postings_lists.entry(word).or_insert_with(Vec::new).extend_from_slice(indexes);
            }

            let words_fst = Set::from_iter(words_fst).unwrap();

            index.main.put_words_fst(&mut writer, &words_fst).unwrap();

            for (word, postings_list) in postings_lists {
                let postings_list = SetBuf::from_dirty(postings_list);
                index.postings_lists.put_postings_list(&mut writer, &word, &postings_list).unwrap();
            }

            writer.commit().unwrap();
            drop(rkv);

            TempDatabase { database, index, _tempdir: tempdir }
        }
    }

    #[test]
    fn simple() {
        let store = TempDatabase::from_iter(vec![
            ("iphone", &[doc_char_index(0, 0, 0)][..]),
            ("from",   &[doc_char_index(0, 1, 1)][..]),
            ("apple",  &[doc_char_index(0, 2, 2)][..]),
        ]);

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "iphone from apple", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, .. }));
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn simple_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("hello", &[doc_index(0, 0)][..]),
        ]);

        store.add_synonym("bonjour", SetBuf::from_dirty(vec!["hello"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "hello", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "bonjour", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn prefix_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("hello", &[doc_index(0, 0)][..]),
        ]);

        store.add_synonym("bonjour", SetBuf::from_dirty(vec!["hello"]));
        store.add_synonym("salut", SetBuf::from_dirty(vec!["hello"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "sal", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "bonj", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "sal blabla", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "bonj blabla", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), None);
    }

    #[test]
    fn levenshtein_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("hello", &[doc_index(0, 0)][..]),
        ]);

        store.add_synonym("salutation", SetBuf::from_dirty(vec!["hello"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "salutution", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "saluttion", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn harder_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("hello",   &[doc_index(0, 0)][..]),
            ("bonjour", &[doc_index(1, 3)]),
            ("salut",   &[doc_index(2, 5)]),
        ]);

        store.add_synonym("hello", SetBuf::from_dirty(vec!["bonjour", "salut"]));
        store.add_synonym("bonjour", SetBuf::from_dirty(vec!["hello", "salut"]));
        store.add_synonym("salut", SetBuf::from_dirty(vec!["hello", "bonjour"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "hello", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 3, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 5, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "bonjour", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 3, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 5, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "salut", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 3, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 5, .. }));
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    /// Unique word has multi-word synonyms
    fn unique_to_multiword_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("new",    &[doc_char_index(0, 0, 0)][..]),
            ("york",   &[doc_char_index(0, 1, 1)][..]),
            ("city",   &[doc_char_index(0, 2, 2)][..]),
            ("subway", &[doc_char_index(0, 3, 3)][..]),

            ("NY",     &[doc_char_index(1, 0, 0)][..]),
            ("subway", &[doc_char_index(1, 1, 1)][..]),
        ]);

        store.add_synonym("NY",  SetBuf::from_dirty(vec!["NYC", "new york", "new york city"]));
        store.add_synonym("NYC", SetBuf::from_dirty(vec!["NY",  "new york", "new york city"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NY ± new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // NY ± york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // NY ± city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true, .. })); // subway
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "NYC subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NYC ± new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // NYC ± york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // NYC ± city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true, .. })); // subway
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn unique_to_multiword_synonyms_words_proximity() {
        let mut store = TempDatabase::from_iter(vec![
            ("new",    &[doc_char_index(0, 0, 0)][..]),
            ("york",   &[doc_char_index(0, 1, 1)][..]),
            ("city",   &[doc_char_index(0, 2, 2)][..]),
            ("subway", &[doc_char_index(0, 3, 3)][..]),

            ("york",   &[doc_char_index(1, 0, 0)][..]),
            ("new",    &[doc_char_index(1, 1, 1)][..]),
            ("subway", &[doc_char_index(1, 2, 2)][..]),

            ("NY",     &[doc_char_index(2, 0, 0)][..]),
            ("subway", &[doc_char_index(2, 1, 1)][..]),
        ]);

        store.add_synonym("NY",  SetBuf::from_dirty(vec!["york new"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. })); // NY ± york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, .. })); // NY ± new
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. })); // york = NY
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, .. })); // new  = NY
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 1, .. })); // york  = NY
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 0, .. })); // new = NY
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, .. })); // york
            assert_matches!(matches.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 1, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 0, .. })); // new
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn unique_to_multiword_synonyms_cumulative_word_index() {
        let mut store = TempDatabase::from_iter(vec![
            ("NY",     &[doc_char_index(0, 0, 0)][..]),
            ("subway", &[doc_char_index(0, 1, 1)][..]),

            ("new",    &[doc_char_index(1, 0, 0)][..]),
            ("york",   &[doc_char_index(1, 1, 1)][..]),
            ("subway", &[doc_char_index(1, 2, 2)][..]),
        ]);

        store.add_synonym("new york", SetBuf::from_dirty(vec!["NY"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NY
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 2, is_exact: true, .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new  = NY
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york = NY
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    /// Unique word has multi-word synonyms
    fn harder_unique_to_multiword_synonyms_one() {
        let mut store = TempDatabase::from_iter(vec![
            ("new",     &[doc_char_index(0, 0, 0)][..]),
            ("york",    &[doc_char_index(0, 1, 1)][..]),
            ("city",    &[doc_char_index(0, 2, 2)][..]),
            ("yellow",  &[doc_char_index(0, 3, 3)][..]),
            ("subway",  &[doc_char_index(0, 4, 4)][..]),
            ("broken",  &[doc_char_index(0, 5, 5)][..]),

            ("NY",      &[doc_char_index(1, 0, 0)][..]),
            ("blue",    &[doc_char_index(1, 1, 1)][..]),
            ("subway",  &[doc_char_index(1, 2, 2)][..]),
        ]);

        store.add_synonym("NY",  SetBuf::from_dirty(vec!["NYC", "new york", "new york city"]));
        store.add_synonym("NYC", SetBuf::from_dirty(vec!["NY",  "new york", "new york city"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true, .. })); // subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true,  .. })); // subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "NYC subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // NYC
            //                                                       because one-word to one-word ^^^^
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true, .. })); // subway
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true,  .. })); // subway
            assert_matches!(iter.next(), None);             // position rewritten ^
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    /// Unique word has multi-word synonyms
    fn even_harder_unique_to_multiword_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("new",         &[doc_char_index(0, 0, 0)][..]),
            ("york",        &[doc_char_index(0, 1, 1)][..]),
            ("city",        &[doc_char_index(0, 2, 2)][..]),
            ("yellow",      &[doc_char_index(0, 3, 3)][..]),
            ("underground", &[doc_char_index(0, 4, 4)][..]),
            ("train",       &[doc_char_index(0, 5, 5)][..]),
            ("broken",      &[doc_char_index(0, 6, 6)][..]),

            ("NY",      &[doc_char_index(1, 0, 0)][..]),
            ("blue",    &[doc_char_index(1, 1, 1)][..]),
            ("subway",  &[doc_char_index(1, 2, 2)][..]),
        ]);

        store.add_synonym("NY",  SetBuf::from_dirty(vec!["NYC", "new york", "new york city"]));
        store.add_synonym("NYC", SetBuf::from_dirty(vec!["NY",  "new york", "new york city"]));
        store.add_synonym("subway", SetBuf::from_dirty(vec!["underground train"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY subway broken", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: false, .. })); // underground = subway
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: false, .. })); // train       = subway
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 5, word_index: 6, is_exact: true,  .. })); // broken
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new  = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // city = NY
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true, .. })); // underground = subway
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: true, .. })); // train       = subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "NYC subway", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // city = NYC
            //                                                       because one-word to one-word ^^^^
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true, .. })); // underground = subway
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: true, .. })); // train       = subway
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new  = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city = NYC
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: false, .. })); // underground = subway
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: false, .. })); // train       = subway
            assert_matches!(iter.next(), None);                // position rewritten ^
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    /// Multi-word has multi-word synonyms
    fn multiword_to_multiword_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("NY",      &[doc_char_index(0, 0, 0)][..]),
            ("subway",  &[doc_char_index(0, 1, 1)][..]),

            ("NYC",     &[doc_char_index(1, 0, 0)][..]),
            ("blue",    &[doc_char_index(1, 1, 1)][..]),
            ("subway",  &[doc_char_index(1, 2, 2)][..]),
            ("broken",  &[doc_char_index(1, 3, 3)][..]),

            ("new",         &[doc_char_index(2, 0, 0)][..]),
            ("york",        &[doc_char_index(2, 1, 1)][..]),
            ("underground", &[doc_char_index(2, 2, 2)][..]),
            ("train",       &[doc_char_index(2, 3, 3)][..]),
            ("broken",      &[doc_char_index(2, 4, 4)][..]),
        ]);

        store.add_synonym("new york", SetBuf::from_dirty(vec![          "NYC", "NY", "new york city" ]));
        store.add_synonym("new york city", SetBuf::from_dirty(vec![     "NYC", "NY", "new york"      ]));
        store.add_synonym("underground train", SetBuf::from_dirty(vec![ "subway"                     ]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york underground train broken", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // underground
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true,  .. })); // train
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 5, word_index: 5, is_exact: true,  .. })); // broken
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // NYC = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // NYC = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // NYC = city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true,  .. })); // subway = underground
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: true,  .. })); // subway = train
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 5, word_index: 6, is_exact: true,  .. })); // broken
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // NY = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // NY = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // NY = city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // subway = underground
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true,  .. })); // subway = train
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york city underground train broken", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // underground
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true,  .. })); // train
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 5, word_index: 5, is_exact: true,  .. })); // broken
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NYC = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // NYC = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // NYC = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // NYC = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // NYC = city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 4, is_exact: true, .. })); // subway = underground
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 5, is_exact: true, .. })); // subway = train
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 5, word_index: 6, is_exact: true, .. })); // broken
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // NY = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // NY = new
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // NY = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // NY = york
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // NY = city
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // subway = underground
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true,  .. })); // subway = train
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn intercrossed_multiword_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("new",   &[doc_index(0, 0)][..]),
            ("york",  &[doc_index(0, 1)][..]),
            ("big",   &[doc_index(0, 2)][..]),
            ("city",  &[doc_index(0, 3)][..]),
        ]);

        store.add_synonym("new york", SetBuf::from_dirty(vec![      "new york city" ]));
        store.add_synonym("new york city", SetBuf::from_dirty(vec![ "new york"      ]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york big ", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: false, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new

            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york

            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 4, is_exact: false, .. })); // city

            assert_matches!(matches.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // big
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);

        let mut store = TempDatabase::from_iter(vec![
            ("NY",     &[doc_index(0, 0)][..]),
            ("city",   &[doc_index(0, 1)][..]),
            ("subway", &[doc_index(0, 2)][..]),

            ("NY",     &[doc_index(1, 0)][..]),
            ("subway", &[doc_index(1, 1)][..]),

            ("NY",     &[doc_index(2, 0)][..]),
            ("york",   &[doc_index(2, 1)][..]),
            ("city",   &[doc_index(2, 2)][..]),
            ("subway", &[doc_index(2, 3)][..]),
        ]);

        store.add_synonym("NY", SetBuf::from_dirty(vec!["new york city story"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "NY subway ", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true, .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true, .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true, .. })); // story
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true, .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 3, is_exact: true,  .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(2), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: false, .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: false, .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // city
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 3, is_exact: true,  .. })); // subway
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn cumulative_word_indices() {
        let mut store = TempDatabase::from_iter(vec![
            ("NYC",    &[doc_index(0, 0)][..]),
            ("long",   &[doc_index(0, 1)][..]),
            ("subway", &[doc_index(0, 2)][..]),
            ("cool",   &[doc_index(0, 3)][..]),
        ]);

        store.add_synonym("new york city", SetBuf::from_dirty(vec!["NYC"]));
        store.add_synonym("subway",        SetBuf::from_dirty(vec!["underground train"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "new york city long subway cool ", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut matches = matches.into_iter();
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 0, word_index: 0, is_exact: true,  .. })); // new  = NYC
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 1, word_index: 1, is_exact: true,  .. })); // york = NYC
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 2, word_index: 2, is_exact: true,  .. })); // city = NYC
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 3, word_index: 3, is_exact: true,  .. })); // long
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 4, word_index: 4, is_exact: true,  .. })); // subway = underground
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 5, word_index: 5, is_exact: true,  .. })); // subway = train
            assert_matches!(matches.next(), Some(TmpMatch { query_index: 6, word_index: 6, is_exact: true,  .. })); // cool
            assert_matches!(matches.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn deunicoded_synonyms() {
        let mut store = TempDatabase::from_iter(vec![
            ("telephone", &[doc_index(0, 0)][..]), // meilidb-data indexes the unidecoded
            ("téléphone", &[doc_index(0, 0)][..]), // and the original words with the same DocIndex

            ("iphone",    &[doc_index(1, 0)][..]),
        ]);

        store.add_synonym("téléphone", SetBuf::from_dirty(vec!["iphone"]));

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "telephone", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "téléphone", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);

        let builder = store.query_builder();
        let results = builder.query(&reader, "télephone", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(1), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, .. }));
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, distance: 1, word_index: 0, is_exact: false, .. })); // iphone
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, distance: 1, word_index: 0, is_exact: false, .. })); // téléphone
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);
    }

    #[test]
    fn simple_concatenation() {
        let store = TempDatabase::from_iter(vec![
            ("iphone",  &[doc_index(0, 0)][..]),
            ("case",    &[doc_index(0, 1)][..]),
        ]);

        let rkv = store.database.rkv.read().unwrap();
        let reader = rkv.read().unwrap();

        let builder = store.query_builder();
        let results = builder.query(&reader, "i phone case", 0..20).unwrap();
        let mut iter = results.into_iter();

        assert_matches!(iter.next(), Some(Document { id: DocumentId(0), matches, .. }) => {
            let mut iter = matches.into_iter();
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 0, word_index: 0, distance: 0, .. })); // iphone
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 1, distance: 0, .. })); // iphone
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 1, word_index: 0, distance: 1, .. })); // phone
            assert_matches!(iter.next(), Some(TmpMatch { query_index: 2, word_index: 2, distance: 0, .. })); // case
            assert_matches!(iter.next(), None);
        });
        assert_matches!(iter.next(), None);
    }
}
