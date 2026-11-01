//! 失败字符光标定位：1 基字符光标 → 1 基（行，列）；位置未知返回 `(0, 0)`。
//!
//! **这一层解决什么**：失败清单里的「第 3 行第 12 列」是运维**唯一**的抓手 ——
//! 它得能照着原文一眼找到那一处。只给一句「解析失败」，运维得把整份报告重读一遍
//! 找可疑数字；给**错了**行列比不给更糟（运维照着错的地方查，查不到就以为系统好了）。
//!
//! **三条纪律**：
//!
//! 1. **按字符计，不按字节** —— UTF-8 多字节字符（中文、生僻字）占**一列**。
//!    按字节算，同一份报告在不同机器上会给出不同列号（字库一换，字节数就变）。
//! 2. **位置不可确定就返回 0** —— 越界、未知一律 `(0, 0)`，**不编造**：
//!    编一个看着合理的行列比留空危险得多（运维会去查一个不存在的地方）。
//! 3. **可反查复核** —— 行列能反查回字符光标（[`char_index_at`]），
//!    失败清单里的每一行都能被**独立复核**回原文（见 `tests/locate_positions.rs`）。
//!
//! **口径约定**（三处容易吵起来的边界，统一在这里定死）：
//!
//! - **换行**：`\n` 起新行；`\r`（CRLF 里的那个）算**上一行的普通字符**，
//!   因此 CRLF 文件的 `\r` 落在该行最后一列。只用 `\r` 换行的老式文件
//!   会被当成一整行 —— 超声工作站不会这么写，不为它单开一套口径。
//! - **制表符**：算**一列**（按字符计，不按显示宽度）。列号是**可复现的定位锚点**，
//!   不是所见即所得的排版坐标；编辑器里看着对不齐是正常的。
//! - **行尾**：指向 `\n` 本身的位置算**上一行的最后一列**（换行符也在那一行里）。
//!
//! **机制溯源（改写，不复制源代码）**：源产品的扫描层给每枚 token 留**字节偏移**
//! （`Token.location`），错误位置再由 `scanner_errposition` 换算成 1 基**字符**光标，
//! 位置未知时返回 0（不编造）。本项目把同一契约落到中文报告上：token 各留 1 基字符光标，
//! 失败条目再由本层换算成 1 基行列；反向的「行列 → 字符光标」是本项目自加的
//! **复核能力**，让导出出去的失败清单可被机器独立验证。

/// 1 基字符光标 → 1 基（行号，列号）。
///
/// - `char_index` 是 **1 基字符**位置（与 [`super::lexer::Token::start`] 同口径）：
///   UTF-8 多字节字符按**一个字符**计，不按字节。
/// - `char_index == 0`（无位置）或越界（指向文本末尾之后）→ 返回 `(0, 0)`
///   —— **位置未知，不编造**。
/// - 列号按**字符**计（制表符占一列），行号按 `\n` 递增。
pub fn locate(text: &str, char_index: usize) -> (usize, usize) {
    if char_index == 0 {
        // 无位置（未知），不是「第一行第一列」。
        return (0, 0);
    }
    let mut line = 1usize;
    let mut column = 1usize;
    for (idx, ch) in text.chars().enumerate() {
        if idx + 1 == char_index {
            return (line, column);
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    // 指向文本末尾之后：位置不可确定。
    (0, 0)
}

/// 1 基（行号，列号） → 1 基字符光标（[`locate`] 的**反查**）。
///
/// 用途是**复核**：失败清单导出的是「行 + 列 + 原文片段」，
/// 拿到原始报告后可以反查回字符光标，确认「这个片段真的在这个位置」——
/// 清单里的每一行都能被独立验证，不必相信解析器。
///
/// - `line == 0` 或 `column == 0`（位置未知）→ `None`。
/// - 行不存在、列超出该行字符数 → `None`（**不编造**）。
/// - 与 [`locate`] 严格互逆：对任一有效字符光标 `i`，
///   `char_index_at(text, locate(text, i).0, locate(text, i).1) == Some(i)`。
pub fn char_index_at(text: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let mut current_line = 1usize;
    let mut current_column = 1usize;
    for (idx, ch) in text.chars().enumerate() {
        if current_line == line && current_column == column {
            return Some(idx + 1);
        }
        if ch == '\n' {
            current_line += 1;
            current_column = 1;
        } else {
            current_column += 1;
        }
    }
    // 末尾之后没有可指向的字符。
    None
}

/// 取（行号，列号）处**起**的若干个字符（复核用：拿它和原文片段比对）。
///
/// 位置未知或越界 → `None`（不返回半截文字冒充原文）。
pub fn snippet_at(text: &str, line: usize, column: usize, len: usize) -> Option<String> {
    let start = char_index_at(text, line, column)?;
    Some(text.chars().skip(start - 1).take(len).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 首行单列() {
        assert_eq!(locate("左肾 3.2cm", 1), (1, 1));
        assert_eq!(locate("左肾 3.2cm", 6), (1, 6));
    }

    #[test]
    fn 多行文本按行累计() {
        let text = "超声所见\n左肾 3.2cm";
        // 第 2 行第 6 个字符
        assert_eq!(locate(text, 11), (2, 6));
    }

    #[test]
    fn 全角字符按一列计() {
        assert_eq!(locate("宽度：３", 4), (1, 4));
    }

    #[test]
    fn 越界与零返回未知() {
        assert_eq!(locate("左肾", 0), (0, 0));
        assert_eq!(locate("左肾", 99), (0, 0));
    }

    #[test]
    fn 空文本没有位置() {
        assert_eq!(locate("", 1), (0, 0));
    }

    #[test]
    fn 换行符本身算上一行的最后一列() {
        let text = "abc\ndef";
        assert_eq!(locate(text, 4), (1, 4), "\\n 属于第一行");
        assert_eq!(locate(text, 5), (2, 1), "下一行从第 1 列起");
    }

    #[test]
    fn 空行不吞掉行号() {
        let text = "a\n\nb";
        assert_eq!(locate(text, 3), (2, 1), "空行的换行符在第 2 行第 1 列");
        assert_eq!(locate(text, 4), (3, 1), "b 在第 3 行第 1 列");
    }

    #[test]
    fn 制表符占一列() {
        let text = "左肾\t积水";
        assert_eq!(locate(text, 3), (1, 3), "制表符按一个字符计");
        assert_eq!(locate(text, 4), (1, 4));
    }

    #[test]
    fn 多字节字符仍占一列() {
        // 「盂」在 UTF-8 里是 3 字节，按字符计仍是一列。
        let text = "左肾盂积水";
        assert_eq!(locate(text, 3), (1, 3));
        assert_eq!(locate(text, 4), (1, 4));
    }

    #[test]
    fn crlf里的回车算上一行普通字符() {
        let text = "超声所见\r\n左肾";
        assert_eq!(locate(text, 5), (1, 5), "\\r 是第一行的第 5 列");
        assert_eq!(locate(text, 6), (1, 6), "\\n 是第一行的第 6 列");
        assert_eq!(locate(text, 7), (2, 1), "下一行从第 1 列起");
    }

    #[test]
    fn 反查与正查互逆() {
        let text = "超声所见\n\n左肾积水 3.2cm\n提示：见上。";
        for i in 1..=text.chars().count() {
            let (line, column) = locate(text, i);
            assert!(
                line >= 1 && column >= 1,
                "字符 {} 的位置不该是未知：{}:{}",
                i,
                line,
                column
            );
            assert_eq!(
                char_index_at(text, line, column),
                Some(i),
                "字符 {} 反查不一致",
                i
            );
        }
    }

    #[test]
    fn 反查未知位置返回空() {
        assert_eq!(char_index_at("左肾", 0, 0), None);
        assert_eq!(char_index_at("左肾", 0, 1), None);
        assert_eq!(char_index_at("左肾", 1, 0), None);
        assert_eq!(char_index_at("左肾", 9, 1), None, "行不存在");
        assert_eq!(char_index_at("左肾", 1, 9), None, "列越界");
    }

    #[test]
    fn 片段按位置取原文() {
        let text = "超声所见\n左肾积水 3.2cm";
        assert_eq!(snippet_at(text, 2, 6, 4).as_deref(), Some("3.2c"));
        assert_eq!(snippet_at(text, 9, 1, 4), None);
    }
}
