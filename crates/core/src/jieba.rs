//! jieba 分词（0036）：jieba-rs 加一层包装，使切分结果与 Python jieba 0.42.1 相同（对照范围见 0036「验证范围」）。
//!
//! jieba-rs 的默认配置与 0.42.1 有四处差异，都在这一层修正：
//! 1. 汉字块：jieba-rs 的字符范围更宽。这里先按 Python 的 `re_han_default` 切块，只把块交给 jieba-rs，
//!    块外按 `re_skip_default` 的规则逐字符输出。
//! 2. 词典与 `total`：使用 0.42.1 的 `dict.txt`。Python 逐行累加 `total`，「B超」重复一行；jieba-rs 去重后
//!    少算，差额用一个永远匹配不到的占位词补齐。
//! 3. HMM：使用全精度模型（`data/jieba/hmm.model`）。jieba-rs 内置的模型把发射概率舍入到 6 位小数。
//! 4. jieba-rs 的 HMM 把 ASCII 的 `.`、`-`、`_` 连成一个词。这里按 finalseg 的 `re_skip` 重新切开，
//!    `cut_for_search` 也自行实现。
//!
//! 用户词按 Python `add_word` 的语义逐项加入（`suggest_freq`、`total` 只增不减、前缀条目的频率为 0），
//! 但每个集合各用一份词典，不复制 3.x 的进程全局状态（0036 决策 C）。

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, OnceLock};

use jieba_rs::{HmmModel, Jieba};

use crate::error::{EngineError, Result};

const DICT: &[u8] = include_bytes!("../data/jieba/dict.txt");
const MODEL: &[u8] = include_bytes!("../data/jieba/hmm.model");

/// Python `gen_pfdict` 逐行累加得到的 `total`。
const DICT_TOTAL: u64 = 60_101_967;
/// jieba-rs 去重后少算的频率：`dict.txt` 里「B超 3 n」重复一行。单元测试 `default_dictionary_loads` 核对。
const DUPLICATED: u64 = 3;
/// 补齐 `total` 的占位词。U+0001 不在 `re_han_default` 内，不会出现在交给 jieba-rs 的块里。
const PLACEHOLDER: &str = "\u{1}";
/// 会进入 trie 的用户词，码位数之和的上限（0036「实施前核实的结果」）。码位数是新增 trie 节点数的上界，
/// 加上默认词典的约 50 万个节点，远低于 jieba-rs 在 `1 << 24` 个节点时的 `assert!`。
pub const MAX_USER_TRIE_CHARS: usize = 1_000_000;

/// `re_han_default = [一-鿕a-zA-Z0-9+#&\._%\-]`。
fn is_han(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fd5}' | 'a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '#' | '&' | '.' | '_' | '%' | '-')
}

fn is_connector(c: char) -> bool {
    matches!(c, '.' | '-' | '_')
}

/// 默认词典的实例。数据是编译期常量，加载只会因构建缺陷失败，单元测试 `default_dictionary_loads` 证明它能加载。
#[allow(clippy::expect_used)]
fn default_jieba() -> &'static Arc<Jieba> {
    static DEFAULT: OnceLock<Arc<Jieba>> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        let mut jieba = Jieba::empty();
        jieba
            .load_dict(&mut &DICT[..])
            .expect("embedded jieba dictionary is well formed");
        jieba.set_hmm_model(
            HmmModel::load(&mut &MODEL[..]).expect("embedded jieba HMM model is well formed"),
        );
        jieba.add_word(PLACEHOLDER, Some(DUPLICATED as usize), None);
        Arc::new(jieba)
    })
}

/// 默认词典按词排序、同词保留最后一行的频率（与 Python `FREQ` 的覆盖顺序相同），只在加用户词时用于查频率与前缀。
#[allow(clippy::expect_used)]
fn dictionary() -> &'static [(&'static str, u64)] {
    static WORDS: OnceLock<Vec<(&'static str, u64)>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let text = std::str::from_utf8(DICT).expect("embedded jieba dictionary is UTF-8");
        let mut words: BTreeMap<&'static str, u64> = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split(' ');
            let word = parts.next().expect("every line has a word");
            let freq = parts
                .next()
                .and_then(|freq| freq.parse().ok())
                .expect("every line has an integer frequency");
            words.insert(word, freq);
        }
        words.into_iter().collect()
    })
}

/// 一个集合的 jieba 分词器：`cut` 或 `cut_for_search`，加上可选的用户词。
#[derive(Clone)]
pub struct Segmenter {
    jieba: Arc<Jieba>,
    search: bool,
    total: u64,
}

impl std::fmt::Debug for Segmenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segmenter")
            .field("search", &self.search)
            .field("total", &self.total)
            .finish()
    }
}

impl Segmenter {
    /// `words` 为空时共用进程级的默认实例；否则从默认词典复制一份，再按列表顺序逐项 `add_word`。
    pub fn new(search: bool, words: Vec<String>) -> Result<Self> {
        if words.is_empty() {
            return Ok(Self {
                jieba: Arc::clone(default_jieba()),
                search,
                total: DICT_TOTAL,
            });
        }
        let mut user = UserDictionary::new();
        for word in &words {
            user.add(word)?;
        }
        Ok(Self {
            total: user.total,
            jieba: Arc::new(user.jieba),
            search,
        })
    }

    /// 词典的 `total`，与 Python 的 `jieba.dt.total` 相同；只供对照工具核对。
    pub fn total(&self) -> u64 {
        self.total
    }

    /// `jieba.cut(text)` 或 `jieba.cut_for_search(text)`，开 HMM。
    pub fn cut<'a>(&self, text: &'a str) -> Vec<&'a str> {
        let words = cut(&self.jieba, text, true);
        if !self.search {
            return words;
        }
        let mut out = Vec::with_capacity(words.len());
        for word in words {
            // 字节偏移：`w[i:i + n]` 按码位取。
            let bounds: Vec<usize> = word
                .char_indices()
                .map(|(at, _)| at)
                .chain(std::iter::once(word.len()))
                .collect();
            let chars = bounds.len() - 1;
            for n in [2, 3] {
                if chars > n {
                    for i in 0..=chars - n {
                        let gram = &word[bounds[i]..bounds[i + n]];
                        // `FREQ.get(gram)` 为真：只有频率大于 0 的词，即 trie 里的词。占位词与不进 trie 的用户词
                        // 都含 `re_han_default` 之外的字符，不可能是汉字块里的子串。
                        if self.jieba.has_word(gram) {
                            out.push(gram);
                        }
                    }
                }
            }
            out.push(word);
        }
        out
    }
}

/// Python `Tokenizer.cut(text, HMM=hmm)`（`cut_all=False`）。
fn cut<'a>(jieba: &Jieba, text: &'a str, hmm: bool) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(first) = rest.chars().next() {
        let han = is_han(first);
        let end = rest
            .char_indices()
            .find(|(_, c)| is_han(*c) != han)
            .map_or(rest.len(), |(at, _)| at);
        let (block, tail) = rest.split_at(end);
        if han {
            cut_block(jieba, block, hmm, &mut out);
        } else {
            // `re_skip_default = (\r\n|\s)` 的 split：分隔符与其余字符都逐个输出，只有 `\r\n` 合成一个。
            let mut chars = block.char_indices().peekable();
            while let Some((at, c)) = chars.next() {
                let len = if c == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
                    chars.next();
                    2
                } else {
                    c.len_utf8()
                };
                out.push(&block[at..at + len]);
            }
        }
        rest = tail;
    }
    out
}

/// 一个汉字块的 `__cut_DAG`（开 HMM）或 `__cut_DAG_NO_HMM`。路径计算与 Python 相同，交给 jieba-rs；
/// 只修正 HMM 输出中被 jieba-rs 用连接符连起来的 ASCII 词（差异 4）。
fn cut_block<'a>(jieba: &Jieba, block: &'a str, hmm: bool, out: &mut Vec<&'a str>) {
    if !hmm {
        out.extend(jieba.cut(block, false).into_iter().map(|token| token.word));
        return;
    }
    for token in jieba.cut(block, true) {
        let word = token.word;
        // 不是词的只能来自 HMM，按 re_skip 切开；是词的由路径给出，原样保留。是词而来自 HMM 的情形不会出现：
        // 那要求路径把这个词的每个字都单独走，单字频率之积超过它的频率。默认词典没有含连接符的词，用户词的频率
        // 来自 `suggest_freq`，加入时就大于它最优切法的乘积；之后单字频率只能靠重复加词逐次加一，而 `total` 增长
        // 得更快，`total` 越大，单字路径越不利（0036「第 1 步的实施结果」）。
        if word.len() > 1
            && word.is_ascii()
            && word.chars().any(is_connector)
            && !jieba.has_word(word)
        {
            skip_split(word, out);
        } else {
            out.push(word);
        }
    }
}

/// finalseg `re_skip = ([a-zA-Z0-9]+(?:\.\d+)?%?)` 的 split 中非空的段：匹配段与其间的间隔依次输出。
/// `word` 是 ASCII，`\d` 只会遇到 ASCII 数字。
fn skip_split<'a>(word: &'a str, out: &mut Vec<&'a str>) {
    let bytes = word.as_bytes();
    let mut gap = 0;
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_alphanumeric() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_alphanumeric() {
            at += 1;
        }
        if at + 1 < bytes.len() && bytes[at] == b'.' && bytes[at + 1].is_ascii_digit() {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
        }
        if at < bytes.len() && bytes[at] == b'%' {
            at += 1;
        }
        if gap < start {
            out.push(&word[gap..start]);
        }
        out.push(&word[start..at]);
        gap = at;
    }
    if gap < bytes.len() {
        out.push(&word[gap..]);
    }
}

fn too_large(what: String) -> EngineError {
    EngineError::invalid(format!(
        "the jieba user dictionary is too large: {what} (limit of milvus-lite-rust)"
    ))
}

/// 加用户词期间的状态，与 Python 的 `FREQ` / `total` 对应。
struct UserDictionary {
    jieba: Jieba,
    /// Python 的 `total`：每加一个词就加上它的频率，旧频率不减。
    total: u64,
    /// jieba-rs 里除占位词外的频率之和。jieba-rs 对已有词先减旧频率，占位词补上它与 `total` 的差。
    trie_total: u64,
    /// 用户加入过的词与当前频率，含不进 trie 的词（Python 的 `FREQ` 里也有它们）。
    words: BTreeMap<String, u64>,
    trie_chars: usize,
}

impl UserDictionary {
    fn new() -> Self {
        Self {
            jieba: Jieba::clone(default_jieba()),
            total: DICT_TOTAL,
            trie_total: DICT_TOTAL - DUPLICATED,
            words: BTreeMap::new(),
            trie_chars: 0,
        }
    }

    /// Python `FREQ.get(word, default)`：词返回频率，只是某个词的前缀返回 0，都不是返回 `default`。
    /// 空串在这里返回 0（它是任何词的前缀），Python 返回 `default`；空串只经 `suggest_freq` 的
    /// `max(total + 1, FREQ.get("", 1))` 用到，两者结果相同。
    fn freq_or(&self, word: &str, default: u64) -> u64 {
        if let Some(freq) = self.words.get(word) {
            return *freq;
        }
        let dictionary = dictionary();
        match dictionary.binary_search_by(|(entry, _)| (*entry).cmp(word)) {
            Ok(at) => return dictionary[at].1,
            // UTF-8 的字节前缀就是码位前缀，排在 `word` 之后的第一个词以它开头，`word` 就是前缀。
            Err(at)
                if dictionary
                    .get(at)
                    .is_some_and(|(entry, _)| entry.starts_with(word)) =>
            {
                return 0;
            }
            Err(_) => {}
        }
        let after = self
            .words
            .range::<str, _>((Bound::Excluded(word), Bound::Unbounded))
            .next();
        if after.is_some_and(|(entry, _)| entry.starts_with(word)) {
            0
        } else {
            default
        }
    }

    /// Python `suggest_freq(word, tune=False)`：逐段乘 `FREQ.get(seg, 1) / float(total)`，乘回 `total` 后截断加一。
    fn suggest_freq(&self, word: &str) -> Result<u64> {
        let ftotal = self.total as f64;
        let mut product: Option<f64> = None;
        for segment in cut(&self.jieba, word, false) {
            let ratio = self.freq_or(segment, 1) as f64 / ftotal;
            product = Some(product.map_or(ratio, |p| p * ratio));
        }
        let suggested = match product {
            // 没有任何段（空串）时 `freq` 仍是整数 1，`int(1 * total) + 1` 按整数计算。
            None => self.total.checked_add(1),
            Some(p) => {
                let scaled = (p * ftotal).trunc();
                // 2^64；`as` 在范围内是精确的。
                if scaled < 18_446_744_073_709_551_616.0 {
                    (scaled as u64).checked_add(1)
                } else {
                    None
                }
            }
        }
        .ok_or_else(|| too_large(format!("a word frequency exceeds {}", u64::MAX)))?;
        Ok(suggested.max(self.freq_or(word, 1)))
    }

    /// Python `add_word(word)`。
    fn add(&mut self, word: &str) -> Result<()> {
        let freq = self.suggest_freq(word)?;
        let total = self
            .total
            .checked_add(freq)
            .ok_or_else(|| too_large(format!("the total frequency exceeds {}", u64::MAX)))?;
        // 含 `re_han_default` 之外字符的词（以及空串）永远不会出现在汉字块里，只影响频率与 `total`，不进 trie。
        if !word.is_empty() && word.chars().all(is_han) {
            self.trie_chars += word.chars().count();
            if self.trie_chars > MAX_USER_TRIE_CHARS {
                return Err(too_large(format!(
                    "words that can be matched exceed {MAX_USER_TRIE_CHARS} characters in total"
                )));
            }
            let old = if self.jieba.has_word(word) {
                self.freq_or(word, 0)
            } else {
                0
            };
            self.jieba.add_word(word, Some(usize_of(freq)?), None);
            self.trie_total = self.trie_total - old + freq;
        }
        self.total = total;
        self.words.insert(word.to_owned(), freq);
        // jieba-rs 改频率时先加新值再减旧值。先清零再设为差额，中间值都不超过新的 `total`，不会溢出。
        self.jieba.add_word(PLACEHOLDER, Some(0), None);
        self.jieba.add_word(
            PLACEHOLDER,
            Some(usize_of(self.total - self.trie_total)?),
            None,
        );
        Ok(())
    }
}

fn usize_of(value: u64) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| too_large(format!("the total frequency exceeds {}", usize::MAX)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cut_with(words: &[&str], search: bool, text: &str) -> Vec<String> {
        let segmenter =
            Segmenter::new(search, words.iter().map(|w| (*w).to_owned()).collect()).unwrap();
        segmenter.cut(text).into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn default_dictionary_loads() {
        let jieba = default_jieba();
        assert!(jieba.has_word("B超"));
        assert!(jieba.has_word(PLACEHOLDER));
        assert_eq!(dictionary().len(), 349_045);
        let deduplicated: u64 = dictionary().iter().map(|(_, freq)| *freq).sum();
        assert_eq!(DICT_TOTAL - deduplicated, DUPLICATED);
    }

    /// 期望值取自 jieba 0.42.1（CPython 3.12）。
    #[test]
    fn matches_python_on_known_sentences() {
        assert_eq!(
            cut_with(&[], false, "我来到北京清华大学"),
            ["我", "来到", "北京", "清华大学"]
        );
        assert_eq!(
            cut_with(&[], true, "小明硕士毕业于中国科学院计算所"),
            [
                "小明",
                "硕士",
                "毕业",
                "于",
                "中国",
                "科学",
                "学院",
                "科学院",
                "中国科学院",
                "计算",
                "计算所"
            ]
        );
        // 差异 4：`a-b` 在 HMM 里按 re_skip 切开；`1.5%` 与 `T.S` 的数字小数规则。
        assert_eq!(cut_with(&[], false, "中a-b"), ["中", "a", "-", "b"]);
        assert_eq!(cut_with(&[], false, "价格1.5%"), ["价格", "1.5%"]);
        // 差异 1：U+9FD6 起不在 Python 的汉字块里，逐字符输出。
        assert_eq!(
            cut_with(&[], false, "\u{9fd6}\u{9fd6}"),
            ["\u{9fd6}", "\u{9fd6}"]
        );
        assert_eq!(
            cut_with(&[], false, "a\r\nb\r\r\n"),
            ["a", "\r\n", "b", "\r", "\r\n"]
        );
    }

    #[test]
    fn skip_split_follows_finalseg() {
        let split = |word| {
            let mut out = Vec::new();
            skip_split(word, &mut out);
            out
        };
        assert_eq!(split("a-b"), ["a", "-", "b"]);
        assert_eq!(split("1.5.6"), ["1.5", ".", "6"]);
        assert_eq!(split("T.S.Eliot"), ["T", ".", "S", ".", "Eliot"]);
        assert_eq!(split("a_b%"), ["a", "_", "b%"]);
        assert_eq!(split("-a-"), ["-", "a", "-"]);
    }

    /// 期望值取自 jieba 0.42.1 的同一个新建 `Tokenizer()`：`total` 与切分逐项相同。
    #[test]
    fn user_words_follow_add_word() {
        let total = |words: &[&str]| {
            Segmenter::new(false, words.iter().map(|w| (*w).to_owned()).collect())
                .unwrap()
                .total()
        };
        // 新词频率为 1；再加一次时它已是词，建议频率为 2，计入 total，旧频率不减。
        assert_eq!(total(&["米尔维斯"]), DICT_TOTAL + 1);
        assert_eq!(total(&["米尔维斯", "米尔维斯"]), DICT_TOTAL + 1 + 2);
        // 空串：`int(1 * total) + 1`，total 翻倍加一。
        assert_eq!(total(&[""]), DICT_TOTAL * 2 + 1);
        assert_eq!(
            cut_with(&["米尔维斯"], false, "我爱米尔维斯"),
            ["我", "爱", "米尔维斯"]
        );
        assert_eq!(
            cut_with(&[], false, "我爱米尔维斯"),
            ["我", "爱", "米尔维", "斯"]
        );
        assert_eq!(
            cut_with(&[""], false, "我爱米尔维斯"),
            ["我", "爱", "米尔维", "斯"]
        );
    }

    #[test]
    fn limits_report_errors_instead_of_panicking() {
        let empties = vec![String::new(); 40];
        let message = Segmenter::new(false, empties).unwrap_err().to_string();
        assert!(
            message.contains("exceeds 18446744073709551615"),
            "{message}"
        );
        // 字符不重复的长词：同一个字重复成的长词会让每个位置的 trie 遍历都走到底（与 Python 的 get_DAG 相同），
        // 测试会慢到不可用。
        let word = |len: usize| -> String {
            (0..len)
                .map(|i| char::from_u32(0x4e00 + (i as u32 * 7919) % 20902).unwrap())
                .collect()
        };
        let message = Segmenter::new(
            false,
            vec![
                word(MAX_USER_TRIE_CHARS / 2 + 1),
                word(MAX_USER_TRIE_CHARS / 2),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("1000000 characters"), "{message}");
        assert!(Segmenter::new(false, vec![word(MAX_USER_TRIE_CHARS / 2); 2]).is_ok());
    }
}
