//! The model's own tokenizer (XLM-R's SentencePiece unigram), read in place from the binary.
//!
//! The steps are the ones the checkpoint's `tokenizer.json` names, in its order:
//! 1. the precompiled character map (SentencePiece's NFKC variant), via `spm_precompiled`;
//! 2. runs of two or more spaces become one;
//! 3. Metaspace: spaces become `▁`, a `▁` is put first, and the text splits before each `▁`;
//! 4. each split is cut into pieces by the unigram model's best path (Viterbi), unknown
//!    characters fused into one `<unk>`;
//! 5. `<s>` and `</s>` around the pieces, after cutting to fit [`crate::MAX_PIECES`].
//!
//! The vocabulary is flat tables in the binary (`build.rs`), so it needs no heap: prefix
//! search is a binary search over piece ids sorted by their text.

use spm_precompiled::Precompiled;

static PIECES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pieces.bin"));
static PIECE_ENDS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/piece_ends.bin"));
static SCORES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scores.bin"));
static SORTED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sorted.bin"));
static CHARSMAP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/charsmap.bin"));

const START: u32 = 0;
const END: u32 = 2;
const UNKNOWN: u32 = 3;
/// SentencePiece's penalty for an unknown character, below the lowest piece score.
const UNKNOWN_PENALTY: f64 = 10.0;
const SPACE: char = '▁';

pub(crate) struct Tokenizer {
    charsmap: Precompiled,
    unknown_score: f64,
}

fn u32_at(table: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(table[index * 4..index * 4 + 4].try_into().expect("u32"))
}

fn pieces() -> usize {
    PIECE_ENDS.len() / 4
}

fn piece(id: u32) -> &'static [u8] {
    let id = id as usize;
    let start = if id == 0 {
        0
    } else {
        u32_at(PIECE_ENDS, id - 1) as usize
    };
    &PIECES[start..u32_at(PIECE_ENDS, id) as usize]
}

fn score(id: u32) -> f64 {
    let at = id as usize * 8;
    f64::from_le_bytes(SCORES[at..at + 8].try_into().expect("f64"))
}

/// The id of the piece whose text is exactly `key`, if any, and whether any piece starts
/// with `key` (so a longer key could still match).
fn find(key: &[u8]) -> (Option<u32>, bool) {
    let (mut low, mut high) = (0usize, pieces());
    while low < high {
        let mid = (low + high) / 2;
        if piece(u32_at(SORTED, mid)) < key {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == pieces() {
        return (None, false);
    }
    let id = u32_at(SORTED, low);
    let text = piece(id);
    if text == key {
        let longer = low + 1 < pieces() && piece(u32_at(SORTED, low + 1)).starts_with(key);
        (Some(id), longer)
    } else {
        (None, text.starts_with(key))
    }
}

impl Tokenizer {
    pub(crate) fn load() -> Result<Self, String> {
        let charsmap = Precompiled::from(CHARSMAP).map_err(|e| e.to_string())?;
        let lowest = (0..pieces() as u32)
            .map(score)
            .fold(f64::INFINITY, f64::min);
        Ok(Self {
            charsmap,
            unknown_score: lowest - UNKNOWN_PENALTY,
        })
    }

    /// The piece ids of `text`, with `<s>` first and `</s>` last, at most `max` in all.
    pub(crate) fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        assert!(max >= 2, "room for the start and end marks");
        let normalized = collapse_spaces(&self.charsmap.normalize_string(text));
        let mut spaced: String = normalized
            .chars()
            .map(|c| if c == ' ' { SPACE } else { c })
            .collect();
        if !spaced.starts_with(SPACE) {
            spaced.insert(0, SPACE);
        }
        let mut ids = vec![START];
        for word in split_before_spaces(&spaced) {
            if ids.len() >= max - 1 {
                break;
            }
            self.cut(word, &mut ids);
        }
        ids.truncate(max - 1);
        ids.push(END);
        ids
    }

    /// The unigram model's best path through `word`, appended to `ids`.
    fn cut(&self, word: &str, ids: &mut Vec<u32>) {
        #[derive(Clone, Copy)]
        struct Best {
            id: u32,
            score: f64,
            from: Option<usize>,
        }
        let bytes = word.as_bytes();
        let mut best = vec![
            Best {
                id: 0,
                score: 0.0,
                from: None
            };
            bytes.len() + 1
        ];
        let mut at = 0;
        while at < bytes.len() {
            let here = best[at].score;
            let char_len = word[at..].chars().next().map_or(1, char::len_utf8);
            let mut single = false;
            let mut end = at + 1;
            while end <= bytes.len() {
                let (id, longer) = find(&bytes[at..end]);
                if let Some(id) = id {
                    let candidate = here + score(id);
                    let node = &mut best[end];
                    if node.from.is_none() || candidate > node.score {
                        *node = Best {
                            id,
                            score: candidate,
                            from: Some(at),
                        };
                    }
                    single |= end - at == char_len;
                }
                if !longer {
                    break;
                }
                end += 1;
            }
            if !single {
                let candidate = here + self.unknown_score;
                let node = &mut best[at + char_len];
                if node.from.is_none() || candidate > node.score {
                    *node = Best {
                        id: UNKNOWN,
                        score: candidate,
                        from: Some(at),
                    };
                }
            }
            at += char_len;
        }
        let mut path = Vec::new();
        let mut end = bytes.len();
        while end > 0 {
            let node = best[end];
            let from = node.from.expect("every end on the path has a start");
            if !(node.id == UNKNOWN && path.last() == Some(&UNKNOWN)) {
                path.push(node.id);
            }
            end = from;
        }
        ids.extend(path.iter().rev());
    }
}

/// Runs of two or more spaces become one space (the `Replace " {2,}"` normalizer).
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_space = false;
    for c in text.chars() {
        if c == ' ' && previous_space {
            continue;
        }
        previous_space = c == ' ';
        out.push(c);
    }
    out
}

/// Split before every `▁`, each `▁` starting the piece after it.
fn split_before_spaces(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if c == SPACE && at > start {
            words.push(&text[start..at]);
            start = at;
        }
    }
    if start < text.len() {
        words.push(&text[start..]);
    }
    words
}
