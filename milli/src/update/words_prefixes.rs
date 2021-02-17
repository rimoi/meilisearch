use std::iter::FromIterator;
use std::str;

use fst::Streamer;
use grenad::CompressionType;
use heed::types::ByteSlice;

use crate::{Index, SmallString32};
use crate::update::index_documents::WriteMethod;
use crate::update::index_documents::{create_sorter, create_writer, writer_into_reader};
use crate::update::index_documents::{word_docids_merge, write_into_lmdb_database};

pub struct WordsPrefixes<'t, 'u, 'i> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    index: &'i Index,
    pub(crate) chunk_compression_type: CompressionType,
    pub(crate) chunk_compression_level: Option<u32>,
    pub(crate) chunk_fusing_shrink_size: Option<u64>,
    pub(crate) max_nb_chunks: Option<usize>,
    pub(crate) max_memory: Option<usize>,
    threshold: f64,
    max_prefix_length: usize,
    _update_id: u64,
}

impl<'t, 'u, 'i> WordsPrefixes<'t, 'u, 'i> {
    pub fn new(
        wtxn: &'t mut heed::RwTxn<'i, 'u>,
        index: &'i Index,
        update_id: u64,
    ) -> WordsPrefixes<'t, 'u, 'i>
    {
        WordsPrefixes {
            wtxn,
            index,
            chunk_compression_type: CompressionType::None,
            chunk_compression_level: None,
            chunk_fusing_shrink_size: None,
            max_nb_chunks: None,
            max_memory: None,
            threshold: 0.01, // 1%
            max_prefix_length: 4,
            _update_id: update_id,
        }
    }

    /// Set the ratio of concerned words required to make a prefix be part of the words prefixes
    /// database. If a word prefix is supposed to match more than this number of words in the
    /// dictionnary, therefore this prefix is added to the words prefixes datastructures.
    ///
    /// Default value is `0.01` or `1%`. This value must be between 0 and 1 and will be clamped
    /// to these bounds otherwise.
    pub fn threshold(&mut self, value: f64) -> &mut Self {
        self.threshold = value.min(1.0).max(0.0); // clamp [0, 1]
        self
    }

    /// Set the maximum length of prefixes in bytes.
    ///
    /// Default value is `4` bytes. This value must be between 1 and 25 will be clamped
    /// to these bounds, otherwise.
    pub fn max_prefix_length(&mut self, value: usize) -> &mut Self {
        self.max_prefix_length = value.min(25).max(1); // clamp [1, 25]
        self
    }

    pub fn execute(self) -> anyhow::Result<()> {
        // Clear the words prefixes datastructures.
        self.index.word_prefix_docids.clear(self.wtxn)?;

        let words_fst = self.index.words_fst(&self.wtxn)?;
        let number_of_words = words_fst.len();
        let min_number_of_words = (number_of_words as f64 * self.threshold) as usize;

        // It is forbidden to keep a mutable reference into the database
        // and write into it at the same time, therefore we write into another file.
        let mut docids_sorter = create_sorter(
            word_docids_merge,
            self.chunk_compression_type,
            self.chunk_compression_level,
            self.chunk_fusing_shrink_size,
            self.max_nb_chunks,
            self.max_memory,
        );

        let mut prefix_fsts = Vec::with_capacity(self.max_prefix_length);
        for n in 1..=self.max_prefix_length {

            let mut current_prefix = SmallString32::new();
            let mut current_prefix_count = 0;
            let mut builder = fst::SetBuilder::memory();

            let mut stream = words_fst.stream();
            while let Some(bytes) = stream.next() {
                // We try to get the first n bytes out of this string but we only want
                // to split at valid characters bounds. If we try to split in the middle of
                // a character we ignore this word and go to the next one.
                let word = str::from_utf8(bytes)?;
                let prefix = match word.get(..n) {
                    Some(prefix) => prefix,
                    None => continue,
                };

                // This is the first iteration of the loop,
                // or the current word doesn't starts with the current prefix.
                if current_prefix_count == 0 || prefix != current_prefix.as_str() {
                    current_prefix = SmallString32::from(prefix);
                    current_prefix_count = 0;
                }

                current_prefix_count += 1;

                // There is enough words corresponding to this prefix to add it to the cache.
                if current_prefix_count == min_number_of_words {
                    builder.insert(prefix)?;
                }
            }

            // We construct the final set for prefixes of size n.
            prefix_fsts.push(builder.into_set());
        }

        // We merge all of the previously computed prefixes into on final set.
        let op = fst::set::OpBuilder::from_iter(prefix_fsts.iter());
        let mut builder = fst::SetBuilder::memory();
        builder.extend_stream(op.r#union())?;
        let prefix_fst = builder.into_set();

        // We iterate over all the prefixes and retrieve the corresponding docids.
        let mut prefix_stream = prefix_fst.stream();
        while let Some(bytes) = prefix_stream.next() {
            let prefix = str::from_utf8(bytes)?;
            let db = self.index.word_docids.remap_data_type::<ByteSlice>();
            for result in db.prefix_iter(self.wtxn, prefix)? {
                let (_word, data) = result?;
                docids_sorter.insert(prefix, data)?;
            }
        }

        // Set the words prefixes FST in the dtabase.
        self.index.put_words_prefixes_fst(self.wtxn, &prefix_fst)?;

        // We write the sorter into a reader to be able to read it back.
        let mut docids_writer = tempfile::tempfile().and_then(|file| {
            create_writer(self.chunk_compression_type, self.chunk_compression_level, file)
        })?;
        docids_sorter.write_into(&mut docids_writer)?;
        let docids_reader = writer_into_reader(docids_writer, self.chunk_fusing_shrink_size)?;

        // We finally write the word prefix docids into the LMDB database.
        write_into_lmdb_database(
            self.wtxn,
            *self.index.word_prefix_docids.as_polymorph(),
            docids_reader,
            word_docids_merge,
            WriteMethod::Append,
        )?;

        Ok(())
    }
}
