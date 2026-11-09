//! OOXML elements that docx-rs cannot currently write directly.

use anyhow::{Context, Result, ensure};
use docx_rs::{Run, XMLDocx};
use pdf_oxide::layout::TextSpan;

pub(crate) const TABLE_CONTINUATION: &str = "PDF table continuation";

#[derive(Default)]
pub(crate) struct WordStructure {
    equations: Vec<String>,
}

fn math_run(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<m:r><m:t xml:space=\"preserve\">{escaped}</m:t></m:r>")
}

fn script_digit(ch: char) -> Option<(char, &'static str)> {
    for (digits, kind) in [("⁰¹²³⁴⁵⁶⁷⁸⁹", "Sup"), ("₀₁₂₃₄₅₆₇₈₉", "Sub")]
    {
        if let Some(digit) = digits.chars().position(|digit| digit == ch) {
            return Some((char::from(b'0' + digit as u8), kind));
        }
    }
    None
}

fn math_expression(text: &str) -> String {
    let mut xml = String::new();
    let mut chars = text.chars().peekable();
    while let Some(base) = chars.next() {
        if let Some((_, kind)) = chars.peek().and_then(|&ch| script_digit(ch)) {
            let mut script = String::new();
            while let Some((digit, _)) = chars
                .next_if(|&ch| script_digit(ch).is_some_and(|(_, next)| next == kind))
                .and_then(script_digit)
            {
                script.push(digit);
            }
            xml.push_str(&format!(
                "<m:s{kind}><m:e>{}</m:e><m:{}>{}</m:{}></m:s{kind}>",
                math_run(&base.to_string()),
                kind.to_ascii_lowercase(),
                math_run(&script),
                kind.to_ascii_lowercase()
            ));
        } else {
            xml.push_str(&math_run(&base.to_string()));
        }
    }
    xml
}

fn math_body(text: &str) -> String {
    if let Some(rows) = text
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
    {
        let rows: Vec<Vec<_>> = rows
            .split(';')
            .map(|row| row.split_whitespace().collect())
            .collect();
        if rows.len() >= 2
            && rows[0].len() >= 2
            && rows.iter().all(|row| row.len() == rows[0].len())
        {
            let cells = rows
                .iter()
                .map(|row| {
                    let cells = row
                        .iter()
                        .map(|cell| format!("<m:e>{}</m:e>", math_expression(cell)))
                        .collect::<String>();
                    format!("<m:mr>{cells}</m:mr>")
                })
                .collect::<String>();
            return format!(
                "<m:d><m:dPr><m:begChr m:val=\"[\"/><m:endChr m:val=\"]\"/></m:dPr><m:e><m:m>{cells}</m:m></m:e></m:d>"
            );
        }
    }
    if let Some((num, den)) = text.split_once('⁄')
        && !num.is_empty()
        && !den.is_empty()
        && !den.contains('⁄')
    {
        return format!(
            "<m:f><m:num>{}</m:num><m:den>{}</m:den></m:f>",
            math_expression(num),
            math_expression(den)
        );
    }
    math_expression(text)
}

impl WordStructure {
    /// Recognize isolated, conservative display-math lines. Preserve uncertain notation as text.
    pub(crate) fn equation(&mut self, line: &[&TextSpan]) -> Option<(Run, String)> {
        let mut text = line
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        if !line.iter().any(|span| {
            let font = span.font_name.to_ascii_lowercase();
            ["math", "cmmi", "cmsy", "cmex", "txsys"]
                .iter()
                .any(|name| font.contains(name))
        }) || !text.chars().any(|ch| "=±×÷∫∑√≤≥≠⁄;".contains(ch))
            || !text.chars().all(|ch| {
                ch.is_ascii() || "±×÷∫∑√≤≥≠⁄−∞·πθαβγλΔ".contains(ch) || script_digit(ch).is_some()
            })
            || text.contains(['<', '>', '\\', '`', ':'])
            || text
                .split(|ch: char| !ch.is_ascii_alphabetic())
                .any(|word| word.len() > 4)
        {
            return None;
        }
        let mut number = String::new();
        if let Some((expression, suffix)) = text.trim_end().rsplit_once('(')
            && let Some(digits) = suffix.strip_suffix(')')
            && digits.chars().any(|ch| ch.is_ascii_digit())
            && digits.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        {
            number = format!(" ({digits})");
            text = expression.trim_end().to_owned();
        }
        let marker = format!("PDF2WordEquationMarker{:08}", self.equations.len());
        self.equations.push(math_body(text.trim()));
        Some((Run::new().add_text(marker), number))
    }

    pub(crate) fn apply(self, mut document: XMLDocx) -> Result<XMLDocx> {
        let mut xml = String::from_utf8(document.document).context("Word 正文 XML 编码无效")?;
        for (index, equation) in self.equations.into_iter().enumerate() {
            let marker = format!(
                "<w:r><w:rPr /><w:t xml:space=\"preserve\">PDF2WordEquationMarker{index:08}</w:t></w:r>"
            );
            ensure!(xml.matches(&marker).count() == 1, "无法定位 Word 公式标记");
            xml = xml.replacen(&marker, &format!(
                "<m:oMath xmlns:m=\"http://schemas.openxmlformats.org/officeDocument/2006/math\">{equation}</m:oMath>"
            ), 1);
        }
        let alias = format!("<w:alias w:val=\"{TABLE_CONTINUATION}\"");
        let mut cursor = 0;
        while let Some(index) = xml[cursor..].find(&alias) {
            let start = cursor + index;
            let Some(end) = xml[start..].find("</w:sdtContent>") else {
                break;
            };
            if let Some(row) = xml[start..start + end].find("</w:trPr>") {
                xml.insert_str(start + row, "<w:tblHeader />");
                cursor = start + end + "<w:tblHeader />".len();
            } else {
                cursor = start + end;
            }
        }
        document.document = xml.into_bytes();
        Ok(document)
    }
}
