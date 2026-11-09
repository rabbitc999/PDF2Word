//! 修正 OCR 英文识别中的多余空格，以及像素证据明确的漏空格。

use std::{collections::HashMap, ops::Range, sync::OnceLock};

use image::RgbImage;

use crate::scan_ocr::PixelRect;

static ENGLISH: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();

fn frequency(word: &str) -> u32 {
    let entries = ENGLISH.get_or_init(|| {
        include_str!("../models/english_word_frequency.txt")
            .lines()
            .filter_map(|line| {
                let (word, count) = line.split_once(' ')?;
                word.bytes()
                    .all(|byte| byte.is_ascii_lowercase())
                    .then_some((word, count.parse::<u32>().ok()?))
            })
            .collect()
    });
    entries.get(word).map_or(0, |&count| count)
}

/// 仅修正 OCR 文本的空格，保留大小写、字母和标点。
pub(crate) fn repair_spacing(image: &RgbImage, bounds: PixelRect, text: &str) -> Option<String> {
    if text.contains("://") || text.contains('@') {
        return None;
    }
    if let Some(joined) = remove_extra_spaces(text) {
        return insert_missing_spaces(image, bounds, &joined).or(Some(joined));
    }
    insert_missing_spaces(image, bounds, text)
}

/// 枚举英文词片段，同时保留原始 UTF-8 字节位置。
fn word_ranges(text: &str) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, byte) in text.bytes().enumerate() {
        if byte.is_ascii_alphabetic() {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            words.push(begin..index);
        }
    }
    if let Some(begin) = start {
        words.push(begin..text.len());
    }
    words
}

fn plain_word(word: &str) -> bool {
    let mut bytes = word.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_lowercase())
}

/// 当片段包含异常词素且连写结果有足够词频时，删除片段之间的单个空格。
fn remove_extra_spaces(text: &str) -> Option<String> {
    let words = word_ranges(text);
    let mut candidates = Vec::new();
    for start in 0..words.len() {
        for count in [2, 3] {
            if let Some(ranges) = words.get(start..start + count)
                && let Some((joined, score)) = merge_candidate(text, ranges)
            {
                candidates.push((start, start + count, joined, score));
            }
        }
    }
    // 相邻候选可能争用同一个片段，优先选择相对原片段更可信的连写词。
    candidates.sort_by(|a, b| b.3.total_cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    let mut used = vec![false; words.len()];
    let mut chosen = Vec::new();
    for (start, end, joined, _) in candidates {
        if !used[start..end].iter().any(|&word| word) {
            used[start..end].fill(true);
            chosen.push((words[start].start..words[end - 1].end, joined));
        }
    }
    chosen.sort_by_key(|(range, _)| range.start);
    replace_ranges(text, chosen)
}

fn replace_ranges(text: &str, replacements: Vec<(Range<usize>, String)>) -> Option<String> {
    if replacements.is_empty() {
        return None;
    }
    let mut cursor = 0;
    let mut output = String::with_capacity(text.len());
    for (range, replacement) in replacements {
        output.push_str(&text[cursor..range.start]);
        output.push_str(&replacement);
        cursor = range.end;
    }
    output.push_str(&text[cursor..]);
    Some(output)
}

fn merge_candidate(text: &str, ranges: &[Range<usize>]) -> Option<(String, f64)> {
    let first = ranges.first()?;
    let last = ranges.last()?;
    let protected = |byte: u8| byte.is_ascii_digit() || matches!(byte, b'_' | b'/' | b'\\' | b'\'');
    if first
        .start
        .checked_sub(1)
        .and_then(|index| text.as_bytes().get(index))
        .is_some_and(|&byte| protected(byte))
        || text
            .as_bytes()
            .get(last.end)
            .is_some_and(|&byte| protected(byte))
        || ranges
            .windows(2)
            .any(|pair| &text[pair[0].end..pair[1].start] != " ")
    {
        return None;
    }
    let fragments: Vec<_> = ranges.iter().map(|range| &text[range.clone()]).collect();
    if !plain_word(fragments[0])
        || fragments
            .iter()
            .skip(1)
            .any(|fragment| !fragment.bytes().all(|byte| byte.is_ascii_lowercase()))
        || fragments
            .iter()
            .any(|fragment| matches!(*fragment, "I" | "A" | "i" | "a"))
        || fragments[0].len() == 1 && fragments[0].as_bytes()[0].is_ascii_uppercase()
    {
        return None;
    }
    let joined = fragments.concat();
    let joined_frequency = frequency(&joined.to_ascii_lowercase());
    if joined_frequency < 250 {
        return None;
    }
    let mut suspicious = false;
    let mut existing_frequency = 1;
    for fragment in fragments {
        let lower = fragment.to_ascii_lowercase();
        let count = frequency(&lower);
        if fragment.len() == 1
            || count == 0
            || matches!(
                lower.as_str(),
                "ing" | "tion" | "tions" | "ation" | "ations"
            )
            || (fragment.len() <= 3 && count <= 500 && joined_frequency >= count.saturating_mul(8))
        {
            suspicious = true;
        } else {
            existing_frequency = existing_frequency.max(count);
        }
    }
    suspicious.then_some((
        joined,
        f64::from(joined_frequency) / f64::from(existing_frequency),
    ))
}

/// 对英文行中的粘连词，只有字形间隙显著且两边都是常用词时才补空格。
fn insert_missing_spaces(image: &RgbImage, bounds: PixelRect, text: &str) -> Option<String> {
    if text.len() > 160
        || bounds.height() < 12
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || byte == b' ')
    {
        return None;
    }

    // 逐列投影墨迹。连笔、标点或粘连字形使边界不明确时，保留原 OCR 结果。
    let mut glyphs = Vec::<Range<u32>>::new();
    let mut start = None;
    let min_ink = (bounds.height() / 42).max(1) as usize;
    for x in bounds.left..bounds.right {
        let dark = (bounds.top..bounds.bottom)
            .filter(|&y| {
                let rgb = image.get_pixel(x, y).0;
                rgb.into_iter().map(u16::from).sum::<u16>() < 630
            })
            .take(min_ink)
            .count()
            == min_ink;
        if dark {
            start.get_or_insert(x);
        } else if let Some(begin) = start.take() {
            glyphs.push(begin..x);
        }
    }
    if let Some(begin) = start {
        glyphs.push(begin..bounds.right);
    }
    let words = word_ranges(text);
    if glyphs.len() != words.iter().map(Range::len).sum::<usize>() {
        // 后面的字形可能相接。若句首之后还有更宽的真实词间距，只校正句首。
        let first = words.first()?.clone();
        let first_glyphs = glyphs.get(..first.len())?;
        let boundary = glyphs.get(first.len())?.start - first_glyphs.last()?.end;
        let widest_inside = first_glyphs
            .windows(2)
            .map(|pair| pair[1].start - pair[0].end)
            .max()?;
        if boundary < widest_inside {
            return None;
        }
        let word = &text[first.clone()];
        let split = split_at_gap(word, first_glyphs, bounds.height())?;
        let (left, right) = word.split_at(split);
        return replace_ranges(text, vec![(first, format!("{left} {right}"))]);
    }
    let mut offset = 0;
    let mut replacements = Vec::new();
    for range in words {
        let word = &text[range.clone()];
        let word_glyphs = &glyphs[offset..offset + word.len()];
        offset += word.len();
        if let Some(split) = split_at_gap(word, word_glyphs, bounds.height()) {
            let (left, right) = word.split_at(split);
            replacements.push((range, format!("{left} {right}")));
        }
    }
    replace_ranges(text, replacements)
}

fn split_at_gap(word: &str, glyphs: &[Range<u32>], height: u32) -> Option<usize> {
    if !(3..=24).contains(&word.len()) || !plain_word(word) {
        return None;
    }
    let mut gaps: Vec<_> = glyphs
        .windows(2)
        .enumerate()
        .map(|(index, pair)| (pair[1].start - pair[0].end, index + 1))
        .collect();
    gaps.sort_unstable();
    let (&(widest, split), others) = gaps.split_last()?;
    let typical = others.get(others.len() / 2)?.0;
    let next_widest = others.last()?.0;
    if (widest as f32) < height as f32 * 0.13
        || widest < typical.saturating_mul(2).saturating_add(1)
        || widest < next_widest.saturating_add(2)
    {
        return None;
    }
    let (left, right) = word.split_at(split);
    let common = |part: &str| {
        let lower = part.to_ascii_lowercase();
        (part.len() > 1 || matches!(lower.as_str(), "a" | "i")) && frequency(&lower) >= 1500
    };
    (common(left) && common(right)).then_some(split)
}

#[cfg(test)]
mod tests {
    use super::remove_extra_spaces;

    #[test]
    fn joins_split_ocr_words_and_keeps_common_phrases() {
        let text = "High-contras t bitmap. Line dra wing. An illus tra tion.";
        assert_eq!(
            remove_extra_spaces(text).as_deref(),
            Some("High-contrast bitmap. Line drawing. An illustration.")
        );
        assert_eq!(
            remove_extra_spaces("Thin vector drawing s ample").as_deref(),
            Some("Thin vector drawing sample")
        );
        for text in [
            "high contrast",
            "in side",
            "every day",
            "some time",
            "a gain",
            "New York",
            "A Report",
            "SKU-42",
        ] {
            assert!(remove_extra_spaces(text).is_none(), "{text}");
        }
    }
}
