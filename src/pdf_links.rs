//! 将 PDF 内部跳转解析为页码和 Word 书签。

use std::collections::{HashMap, HashSet};

use pdf_oxide::{
    PdfDocument,
    annotation_types::AnnotationSubtype,
    annotations::{Annotation, LinkAction},
    object::{Object, ObjectRef},
};

#[derive(Default)]
pub(crate) struct PdfLinks {
    pub(crate) annotations: HashMap<usize, Vec<Annotation>>,
    pub(crate) anchors: HashMap<usize, Vec<Option<usize>>>,
    pub(crate) bookmarks: HashMap<usize, Vec<(usize, Option<f32>)>>,
}

pub(crate) fn bookmark_name(id: usize) -> String {
    format!("PdfLink{id}")
}

fn entry<'a>(object: &'a Object, key: &str) -> Option<&'a Object> {
    object.as_dict()?.get(key)
}

fn page_refs(
    pdf: &PdfDocument,
    node: &Object,
    pages: &mut Vec<Option<ObjectRef>>,
    visited: &mut HashSet<ObjectRef>,
    depth: usize,
) {
    if depth == 64 || node.as_reference().is_some_and(|r| !visited.insert(r)) {
        return;
    }
    let Ok(resolved) = pdf.resolve_object(node) else {
        return;
    };
    match entry(&resolved, "Type").and_then(Object::as_name) {
        Some("Page") => pages.push(node.as_reference()),
        Some("Pages") => {
            if let Some(kids) =
                entry(&resolved, "Kids").and_then(|kids| pdf.resolve_object(kids).ok())
                && let Some(kids) = kids.as_array()
            {
                for kid in kids {
                    page_refs(pdf, kid, pages, visited, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn read_name_tree(
    pdf: &PdfDocument,
    node: &Object,
    names: &mut HashMap<Vec<u8>, Object>,
    visited: &mut HashSet<ObjectRef>,
    depth: usize,
) {
    if depth == 32 || node.as_reference().is_some_and(|r| !visited.insert(r)) {
        return;
    }
    let Ok(node) = pdf.resolve_object(node) else {
        return;
    };
    if let Some(entries) = entry(&node, "Names").and_then(Object::as_array) {
        for pair in entries.chunks_exact(2) {
            if let Some(name) = pair[0].as_string() {
                names.insert(name.to_vec(), pair[1].clone());
            }
        }
    }
    if let Some(kids) = entry(&node, "Kids").and_then(Object::as_array) {
        for kid in kids {
            read_name_tree(pdf, kid, names, visited, depth + 1);
        }
    }
}

fn destination(
    pdf: &PdfDocument,
    annotation: &Annotation,
    names: &HashMap<Vec<u8>, Object>,
    pages: &HashMap<ObjectRef, usize>,
) -> Option<(usize, Option<f32>)> {
    if annotation.subtype_enum != AnnotationSubtype::Link {
        return None;
    }
    let dict = annotation.raw_dict.as_ref()?;
    let raw = if matches!(annotation.action, Some(LinkAction::GoTo(_))) {
        let action = pdf.resolve_object(dict.get("A")?).ok()?;
        entry(&action, "D")?.clone()
    } else {
        dict.get("Dest")?.clone()
    };
    let mut resolved = pdf.resolve_object(&raw).ok()?;
    if let Some(name) = resolved
        .as_string()
        .or_else(|| resolved.as_name().map(str::as_bytes))
    {
        resolved = pdf.resolve_object(names.get(name)?).ok()?;
    }
    if let Some(inner) = entry(&resolved, "D") {
        resolved = pdf.resolve_object(inner).ok()?;
    }
    let array = resolved.as_array()?;
    let target = array.first()?;
    let page = match target.as_reference() {
        Some(reference) => *pages.get(&reference)?,
        None => usize::try_from(target.as_integer()?).ok()?,
    };
    let top_index = match array.get(1).and_then(Object::as_name) {
        Some("XYZ") => Some(3),
        Some("FitH" | "FitBH") => Some(2),
        Some("FitR") => Some(5),
        _ => None,
    };
    let top = top_index
        .and_then(|index| array.get(index))
        .and_then(|value| {
            value
                .as_real()
                .or_else(|| value.as_integer().map(|value| value as f64))
        })
        .map(|value| value as f32)
        .filter(|value| value.is_finite());
    Some((page, top))
}

impl PdfLinks {
    pub(crate) fn read(pdf: &PdfDocument, selected: &[usize]) -> Self {
        let mut links = Self::default();
        let mut page_objects = Vec::new();
        let mut names = HashMap::new();
        if let Ok(catalog) = pdf.catalog() {
            if let Some(tree) = entry(&catalog, "Pages") {
                page_refs(pdf, tree, &mut page_objects, &mut HashSet::new(), 0);
            }
            if let Some(dests) =
                entry(&catalog, "Dests").and_then(|node| pdf.resolve_object(node).ok())
                && let Some(dests) = dests.as_dict()
            {
                names.extend(
                    dests
                        .iter()
                        .map(|(name, value)| (name.as_bytes().to_vec(), value.clone())),
                );
            }
            if let Some(tree) =
                entry(&catalog, "Names").and_then(|node| pdf.resolve_object(node).ok())
                && let Some(dests) = entry(&tree, "Dests")
            {
                read_name_tree(pdf, dests, &mut names, &mut HashSet::new(), 0);
            }
        }
        let pages: HashMap<_, _> = page_objects
            .into_iter()
            .enumerate()
            .filter_map(|(index, page)| page.map(|page| (page, index)))
            .collect();
        let mut ids = HashMap::new();
        for &page in selected {
            let annotations = match pdf.get_annotations(page) {
                Ok(annotations) => annotations,
                Err(error) => {
                    eprintln!("提示：无法读取第 {} 页的链接和批注：{error}", page + 1);
                    continue;
                }
            };
            let anchors = annotations
                .iter()
                .map(|annotation| {
                    let (target, top) = destination(pdf, annotation, &names, &pages)?;
                    selected.binary_search(&target).ok()?;
                    let next_id = ids.len() + 1;
                    Some(
                        *ids.entry((target, top.map(f32::to_bits)))
                            .or_insert_with(|| {
                                links
                                    .bookmarks
                                    .entry(target)
                                    .or_default()
                                    .push((next_id, top));
                                next_id
                            }),
                    )
                })
                .collect();
            links.anchors.insert(page, anchors);
            links.annotations.insert(page, annotations);
        }
        links
    }
}
