//! 解析内核第 1 层：词法扫描器（token 流 + 起始字符光标）。
//!
//! 机制溯源（**改写**，不复制源代码）：源产品的解析驱动层把每条输入切成 token 流，
//! 每个 token 保留输入缓冲区的位置；错误位置再由字节偏移换算为 1 基字符光标
//! （`scanner_errposition`）。本项目把同一机制改写为**中文超声报告词表**：
//! 按字符类别切 token，每个 token 记录 1 基字符光标（UTF-8 多字节按字符计，不按字节），
//! 供失败条目直接指回原文第几行第几列。
//!
//! **词法层四条纪律**：
//!
//! 1. **字符光标按字符计，不按字节** —— 多字节汉字与全角字符各占 1 列。
//! 2. **全角归一只发生在数值与字母上** —— 全角数字归一为半角数值 token；
//!    全角标点**原样保留**、各自独立成 token（标点是位置锚点，归一了就指不回原文）。
//! 3. **词典没收录的中文不猜** —— 连续汉字聚成一个 `Word` token，
//!    绝不猜成部位或测项（猜错会让对拍「看起来正常却比错了对象」）。
//! 4. **单位词面整块切分** —— `cm3` / `cm³` / `㎤` 里的指数是**单位的一部分**：
//!    切成「单位 cm + 独立数值 3」会把 2.5cm³ 读成 2.5cm（25mm），
//!    台账里的数值看着正常，却比错了量纲。
//!
//! **本层不产出条目**：token 流只是原料，部位 / 测项 / 数值 / 单位的组装在
//! 第 2–6 层（合并与抽取）。倍率写法（`3.2×2cm`）在词法层只保留
//! `数值 / 乘号 / 数值 / 单位` 四枚 token，**合并规则留给前瞻合并表**（第 2 层）。

use crate::coerce::units::{
    compat_unit, is_fullwidth_latin, normalize_unit_spelling, superscript_digit,
};

/// token 类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// 数值（全角数字已归一为半角）。
    Number,
    /// 单位候选（**已知或未知都保留原样**：未知单位必须能进抽取层，
    /// 否则「查不到换算路径」会被词法层悄悄吃掉）。
    Unit,
    /// 标点 / 分隔符（半角与全角都原样保留，各自独立成 token）。
    Punct,
    /// 乘号（`×` / `x`）—— 倍率写法的材料，**本层不做合并**。
    Multiply,
    /// 侧别修饰词（左 / 右 / 左侧 / 右侧 / 双侧）。
    Modifier,
    /// 部位词（肾 / 肾盂 / 输尿管 / 膀胱 …）。
    Part,
    /// 测项词（长 / 宽 / 前后径 / 残余尿量 …）。
    Item,
    /// 段标题（超声所见 / 提示 / 结论）。
    Section,
    /// 其他实义词（积水 / 囊肿 / 结石 …）。
    Term,
    /// 未识别词。
    Word,
}

impl TokenKind {
    /// 类别字符串（进人工标注表与调试输出）。
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Number => "Number",
            TokenKind::Unit => "Unit",
            TokenKind::Punct => "Punct",
            TokenKind::Multiply => "Multiply",
            TokenKind::Modifier => "Modifier",
            TokenKind::Part => "Part",
            TokenKind::Item => "Item",
            TokenKind::Section => "Section",
            TokenKind::Term => "Term",
            TokenKind::Word => "Word",
        }
    }
}

/// 一个词法 token。
#[derive(Clone, Debug)]
pub struct Token {
    /// token 类别。
    pub kind: TokenKind,
    /// 归一后的词面（数值已把全角数字转半角；单位已把方块兼容字符与上标转 ASCII）。
    pub text: String,
    /// 原文词面（不做全角归一，供失败清单回显原文片段）。
    pub raw: String,
    /// 1 基字符光标 —— token 起始位置。
    pub start: usize,
    /// 1 基字符光标 —— token 结束位置（含）。
    pub end: usize,
}

/// 中文词表：**最长匹配优先**把中文串切成 token。
///
/// 词表只覆盖泌尿系超声首版范围的常见写法；未命中的中文按连续字符聚成 `Word`，
/// 绝不猜成部位或测项（猜错会让对拍「看起来正常却比错了对象」）。
const LEXICON: &[(&str, TokenKind)] = &[
    // 段标题
    ("超声所见", TokenKind::Section),
    ("检查所见", TokenKind::Section),
    ("超声描述", TokenKind::Section),
    ("超声结论", TokenKind::Section),
    ("声像图", TokenKind::Section),
    ("超声提示", TokenKind::Section),
    ("所见", TokenKind::Section),
    ("提示", TokenKind::Section),
    ("结论", TokenKind::Section),
    // 侧别修饰词
    ("双侧", TokenKind::Modifier),
    ("左侧", TokenKind::Modifier),
    ("右侧", TokenKind::Modifier),
    // 「双」是「双侧」的简写（「双肾」≡「双侧肾」）—— 合并层不把它与部位词烧成词面，
    // 侧别判定留给抽取层。
    ("双", TokenKind::Modifier),
    ("左", TokenKind::Modifier),
    ("右", TokenKind::Modifier),
    // 部位词
    ("输尿管", TokenKind::Part),
    ("肾脏", TokenKind::Part),
    ("肾盂", TokenKind::Part),
    ("肾盏", TokenKind::Part),
    ("膀胱", TokenKind::Part),
    // 「胆囊」是**泌尿系口径之外**的部位：词法层认得出它是部位词，
    // 但部位词典查不到 —— 抽取层据此产出未命中（不参与对拍），
    // 而不是把它悄悄当成某个泌尿系器官。
    ("胆囊", TokenKind::Part),
    ("肾", TokenKind::Part),
    // 测项词
    ("残余尿量", TokenKind::Item),
    ("前后径", TokenKind::Item),
    ("长度", TokenKind::Item),
    ("宽度", TokenKind::Item),
    ("内径", TokenKind::Item),
    ("长", TokenKind::Item),
    ("宽", TokenKind::Item),
    // 其他实义词（不参与部位/测项抽取，只作为报告叙述保留）
    ("积水", TokenKind::Term),
    ("囊肿", TokenKind::Term),
    ("结石", TokenKind::Term),
    ("阴影", TokenKind::Term),
    ("充盈", TokenKind::Term),
    ("排空", TokenKind::Term),
    ("实质", TokenKind::Term),
    ("壁", TokenKind::Term),
    // 中文单位别名
    ("立方厘米", TokenKind::Unit),
    ("厘米", TokenKind::Unit),
    ("毫米", TokenKind::Unit),
    ("毫升", TokenKind::Unit),
    ("升", TokenKind::Unit),
];

/// 词表最长词长（`残余尿量` / `超声所见` 各 4 字，留 6 字余量）。
const MAX_LEX_LEN: usize = 6;

/// 判断是否为汉字。
///
/// 覆盖**四个区**：基本区、扩展 A（`㐂` 这类生僻字与旧系统异体字）、
/// 兼容表意文字（`豈` 这类由旧编码转来的字形）、扩展 B/C（`𠀀` 这类生僻字）。
///
/// 只认基本区会让扩展区汉字**掉进标点分支** —— 一个字变成一枚噪声标点 token，
/// 还会把后面的字符光标带偏。
///
/// 文档闸门（[`super::gate`]）也用这一处判定 —— 「什么算汉字」只有一份实现，
/// 两处各写一份必然漂移（漂移表现为：闸门判成中文、词法层却按标点切）。
pub(crate) fn is_han(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF      // 基本区
        | 0x3400..=0x4DBF    // 扩展 A
        | 0xF900..=0xFAFF    // 兼容表意文字
        | 0x20000..=0x2A6DF  // 扩展 B
        | 0x2A700..=0x2EBEF  // 扩展 C–F
    )
}

/// 是否为全角数字（U+FF10–U+FF19）。
///
/// 公开给数值层复用：全角数字与半角数字**是同一件事**，
/// 归一只做一次（词法层与数值层共用这一处实现，不各写一份）。
pub fn is_fullwidth_digit(c: char) -> bool {
    matches!(c as u32, 0xFF10..=0xFF19)
}

/// 全角数字 → 半角数字字符。
pub fn to_halfwidth_digit(c: char) -> char {
    char::from_u32(c as u32 - 0xFF10 + 0x30).unwrap_or(c)
}

/// 是否为数字（半角或全角）。
fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || is_fullwidth_digit(c)
}

/// 是否为拉丁字母（半角或全角）。
fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic() || is_fullwidth_latin(c)
}

/// 是否为乘号（`×` / `✕`）。
fn is_multiply_sign(c: char) -> bool {
    matches!(c as u32, 0xD7 | 0x2715)
}

/// 是否为可跳过字符（空白 + 零宽字符）。
///
/// 零宽字符（BOM / 零宽空格）在旧工作站导出的报告里真实存在；
/// 当作空白跳过，而不是变成噪声标点。
fn is_skippable(c: char) -> bool {
    c.is_whitespace() || matches!(c as u32, 0x200B | 0x2060 | 0xFEFF)
}

/// 词表最长匹配：在 `pos` 处查词表，命中返回（词长, 类别）。
fn lexicon_lookup(chars: &[char], pos: usize) -> Option<(usize, TokenKind)> {
    let max_len = (chars.len() - pos).min(MAX_LEX_LEN);
    for len in (1..=max_len).rev() {
        let cand: String = chars[pos..pos + len].iter().collect();
        if let Some((_, kind)) = LEXICON.iter().find(|(s, _)| *s == cand.as_str()) {
            return Some((len, *kind));
        }
    }
    None
}

/// 词法扫描器。
struct Scanner {
    /// 正文字符（按字符切，不按字节 —— 光标口径的基础）。
    chars: Vec<char>,
    /// 当前游标（0 基）。
    pos: usize,
}

impl Scanner {
    fn new(text: &str) -> Self {
        Scanner {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    /// 扫完全文，返回 token 序列。
    fn run(&mut self) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::new();
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            if is_skippable(c) {
                self.pos += 1;
                continue;
            }
            if is_digit(c) {
                out.push(self.scan_number());
                continue;
            }
            if c == '.' || c == '\u{ff0e}' {
                out.push(self.scan_punct());
                continue;
            }
            if is_latin(c) || compat_unit(c).is_some() {
                // 「3.2x2cm」里的 x 是乘号，不是单位。
                if self.is_multiply_letter(self.pos) {
                    out.push(self.scan_multiply());
                } else {
                    out.push(self.scan_unit());
                }
                continue;
            }
            if is_han(c) {
                out.extend(self.scan_han());
                continue;
            }
            if is_multiply_sign(c) {
                out.push(self.scan_multiply());
                continue;
            }
            out.push(self.scan_punct());
        }
        out
    }

    /// 数值：半角 / 全角数字 + 小数点（多个小数点也照收，
    /// 由抽取层判 VALUE_UNPARSEABLE，不在词法层猜值）。
    fn scan_number(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            if is_digit(c) || c == '.' || c == '\u{ff0e}' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        let text: String = raw
            .chars()
            .map(|c| match c {
                '\u{ff0e}' => '.',
                d if is_fullwidth_digit(d) => to_halfwidth_digit(d),
                o => o,
            })
            .collect();
        Token {
            kind: TokenKind::Number,
            text,
            raw,
            start: start + 1,
            end: self.pos,
        }
    }

    /// 单位词面：兼容方块单位（`㎝`）/ 拉丁字母串（可带指数）。
    ///
    /// **指数属于单位**：`cm3` / `cm³` / `cm^3` / `㎤` 都切成一枚单位 token；
    /// 切错会把 2.5cm³ 读成 2.5cm（25mm），台账数值看着正常却比错了量纲。
    fn scan_unit(&mut self) -> Token {
        let start = self.pos;

        // 兼容方块单位：整体即一个单位词面。
        if let Some(rep) = compat_unit(self.chars[self.pos]) {
            self.pos += 1;
            let raw: String = self.chars[start..self.pos].iter().collect();
            return Token {
                kind: TokenKind::Unit,
                text: rep.to_ascii_lowercase(),
                raw,
                start: start + 1,
                end: self.pos,
            };
        }

        // 拉丁字母串（半角 + 全角）。
        while self.pos < self.chars.len() && is_latin(self.chars[self.pos]) {
            self.pos += 1;
        }

        // 指数部分：上标数字 / `^数字` / 紧跟的单个数字（`cm3`）。
        if let Some(&c) = self.chars.get(self.pos) {
            if superscript_digit(c).is_some() {
                self.pos += 1;
            } else if c == '^' {
                if let Some(&d) = self.chars.get(self.pos + 1) {
                    if d.is_ascii_digit() {
                        self.pos += 2;
                    }
                }
            } else if c.is_ascii_digit() {
                // 只有一个数字才是指数：`cm3` 是 cm³，`cm30` 是「单位 cm + 数值 30」。
                let next_is_digit = self
                    .chars
                    .get(self.pos + 1)
                    .map(|d| d.is_ascii_digit())
                    .unwrap_or(false);
                if !next_is_digit {
                    self.pos += 1;
                }
            }
        }

        let raw: String = self.chars[start..self.pos].iter().collect();
        let text = normalize_unit_spelling(&raw).to_ascii_lowercase();
        Token {
            kind: TokenKind::Unit,
            text,
            raw,
            start: start + 1,
            end: self.pos,
        }
    }

    /// 中文串：按词表最长匹配切分；未命中的连续汉字聚成 `Word`。
    fn scan_han(&mut self) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::new();
        let mut word_start = self.pos;
        let mut word_buf = String::new();
        while self.pos < self.chars.len() && is_han(self.chars[self.pos]) {
            match lexicon_lookup(&self.chars, self.pos) {
                Some((len, kind)) => {
                    if !word_buf.is_empty() {
                        out.push(Token {
                            kind: TokenKind::Word,
                            text: word_buf.clone(),
                            raw: word_buf.clone(),
                            start: word_start + 1,
                            end: self.pos,
                        });
                        word_buf.clear();
                    }
                    let text: String = self.chars[self.pos..self.pos + len].iter().collect();
                    out.push(Token {
                        kind,
                        text: text.clone(),
                        raw: text,
                        start: self.pos + 1,
                        end: self.pos + len,
                    });
                    self.pos += len;
                    word_start = self.pos;
                }
                None => {
                    if word_buf.is_empty() {
                        word_start = self.pos;
                    }
                    word_buf.push(self.chars[self.pos]);
                    self.pos += 1;
                }
            }
        }
        if !word_buf.is_empty() {
            out.push(Token {
                kind: TokenKind::Word,
                text: word_buf.clone(),
                raw: word_buf.clone(),
                start: word_start + 1,
                end: self.pos,
            });
        }
        out
    }

    /// 标点 / 分隔符：半角与全角**原样保留**，各自独立成 token。
    fn scan_punct(&mut self) -> Token {
        let c = self.chars[self.pos];
        self.pos += 1;
        Token {
            kind: TokenKind::Punct,
            text: c.to_string(),
            raw: c.to_string(),
            start: self.pos,
            end: self.pos,
        }
    }

    /// 乘号 token（倍率写法的材料；合并规则在第 2 层前瞻合并表）。
    fn scan_multiply(&mut self) -> Token {
        let c = self.chars[self.pos];
        self.pos += 1;
        Token {
            kind: TokenKind::Multiply,
            text: c.to_string(),
            raw: c.to_string(),
            start: self.pos,
            end: self.pos,
        }
    }

    /// 「3.2x2cm」里的 `x` 是乘号而不是单位：单个拉丁字母 x/X，左右都是数字。
    fn is_multiply_letter(&self, pos: usize) -> bool {
        let c = self.chars[pos];
        if c != 'x' && c != 'X' {
            return false;
        }
        // 字母串必须只有这一个字母（`cm` 不是乘号）。
        if self.chars.get(pos + 1).map(|c| is_latin(*c)).unwrap_or(false) {
            return false;
        }
        let prev_digit = pos > 0 && is_digit(self.chars[pos - 1]);
        let next_digit = self
            .chars
            .get(pos + 1)
            .map(|c| is_digit(*c))
            .unwrap_or(false);
        prev_digit && next_digit
    }
}

/// 把报告正文切成 token 序列。
///
/// 每个 token 记录 1 基字符光标（`start` / `end`），后续失败条目直接引用。
pub fn tokenize(text: &str) -> Vec<Token> {
    Scanner::new(text).run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 切出部位测项数值单位四类token() {
        let toks = tokenize("左肾 3.2cm");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Modifier,
                TokenKind::Part,
                TokenKind::Number,
                TokenKind::Unit
            ]
        );
        assert_eq!(toks[2].text, "3.2");
        assert_eq!(toks[3].raw, "cm");
    }

    #[test]
    fn 全角数字归一为半角() {
        let toks = tokenize("３.２cm");
        assert_eq!(toks[0].kind, TokenKind::Number);
        assert_eq!(toks[0].text, "3.2");
        assert_eq!(toks[0].raw, "３.２");
    }

    #[test]
    fn 全角小数点归一为半角() {
        let toks = tokenize("３．２cm");
        assert_eq!(toks[0].kind, TokenKind::Number);
        assert_eq!(toks[0].text, "3.2");
        assert_eq!(toks[0].raw, "３．２");
    }

    #[test]
    fn 全角标点原样保留且独立成token() {
        let toks = tokenize("左肾：3.2cm");
        assert_eq!(toks[2].kind, TokenKind::Punct);
        assert_eq!(toks[2].text, "：");
        assert_eq!(toks[2].raw, "：");
        assert_eq!(toks[2].start, 3);
        assert_eq!(toks[2].end, 3);
    }

    #[test]
    fn 字符光标按字符计不按字节() {
        let text = "肾盂积水 2.1cm";
        let toks = tokenize(text);
        let num = toks.iter().find(|t| t.kind == TokenKind::Number).unwrap();
        // 「肾盂积水 2.1cm」：第 6 个字符是 '2'
        assert_eq!(num.start, 6);
        assert_eq!(num.end, 8);
    }

    #[test]
    fn 未识别中文聚成词且不猜成部位() {
        let toks = tokenize("肝实质回声");
        assert!(toks.iter().all(|t| t.kind == TokenKind::Word || t.kind == TokenKind::Term));
        assert!(!toks.iter().any(|t| t.kind == TokenKind::Part));
    }

    #[test]
    fn 侧别与部位最长匹配优先() {
        let toks = tokenize("左侧肾脏");
        assert_eq!(toks[0].kind, TokenKind::Modifier);
        assert_eq!(toks[0].text, "左侧");
        assert_eq!(toks[1].kind, TokenKind::Part);
        assert_eq!(toks[1].text, "肾脏");
    }

    #[test]
    fn 段标题与修饰词各自独立() {
        let toks = tokenize("超声所见：左侧肾脏");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Section,
                TokenKind::Punct,
                TokenKind::Modifier,
                TokenKind::Part
            ]
        );
        assert_eq!(toks[0].text, "超声所见");
    }

    #[test]
    fn 复合量纲单位整块切分不拆指数() {
        // cm3：指数属于单位，不能切成「cm + 3」。
        let toks = tokenize("膀胱 2.5cm3");
        let unit = toks.last().unwrap();
        assert_eq!(unit.kind, TokenKind::Unit);
        assert_eq!(unit.text, "cm3");
        assert_eq!(unit.raw, "cm3");
        assert_eq!(toks.len(), 3);
    }

    #[test]
    fn 上标与幂写法归一到同一单位词面() {
        for raw in ["cm³", "cm^3", "㎤"] {
            let toks = tokenize(&format!("2.5{}", raw));
            let unit = toks.last().unwrap();
            assert_eq!(unit.kind, TokenKind::Unit, "{} 应切出单位 token", raw);
            assert_eq!(unit.text, "cm3", "{} 应归一到 cm3", raw);
        }
    }

    #[test]
    fn 单位后跟两位数字不并入指数() {
        // `cm30` 是「单位 cm + 数值 30」，不是 cm³⁰。
        let toks = tokenize("2.5cm30");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![TokenKind::Number, TokenKind::Unit, TokenKind::Number]
        );
        assert_eq!(toks[1].text, "cm");
        assert_eq!(toks[2].text, "30");
    }

    #[test]
    fn 兼容方块单位归一但保留原字面() {
        let toks = tokenize("右肾 3.2㎝");
        let unit = toks.last().unwrap();
        assert_eq!(unit.kind, TokenKind::Unit);
        assert_eq!(unit.text, "cm");
        assert_eq!(unit.raw, "㎝");
    }

    #[test]
    fn 乘号独立成token且不并进单位() {
        let toks = tokenize("右肾 3.2×2cm");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Modifier,
                TokenKind::Part,
                TokenKind::Number,
                TokenKind::Multiply,
                TokenKind::Number,
                TokenKind::Unit
            ]
        );
        assert_eq!(toks[3].text, "×");
    }

    #[test]
    fn 数字之间的x是乘号而不是单位() {
        let toks = tokenize("3.2x2cm");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Number,
                TokenKind::Multiply,
                TokenKind::Number,
                TokenKind::Unit
            ]
        );
        assert_eq!(toks[1].text, "x");
    }

    #[test]
    fn 生僻字不落标点分支() {
        // 扩展 A 的「㐂」与兼容表意文字「豈」：都是汉字，不是标点。
        let toks = tokenize("㐂豈");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Word);
        assert_eq!(toks[0].text, "㐂豈");
        assert_eq!(toks[0].start, 1);
        assert_eq!(toks[0].end, 2);
    }

    #[test]
    fn 零宽字符按空白跳过不产生噪声token() {
        let toks = tokenize("左肾\u{200b} 3.2cm");
        assert_eq!(toks.len(), 4);
        assert_eq!(toks[0].kind, TokenKind::Modifier);
    }

    #[test]
    fn 空文本不产生token() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   \n\t").is_empty());
    }
}
