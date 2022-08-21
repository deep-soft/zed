use crate::{
    Grammar, InjectionConfig, Language, LanguageRegistry, QueryCursorHandle, TextProvider,
    ToTreeSitterPoint,
};
use std::{
    borrow::Cow, cell::RefCell, cmp::Ordering, collections::BinaryHeap, ops::Range, sync::Arc,
};
use sum_tree::{Bias, SeekTarget, SumTree};
use text::{Anchor, BufferSnapshot, OffsetRangeExt, Point, Rope, ToOffset, ToPoint};
use tree_sitter::{Parser, Tree};

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

#[derive(Default)]
pub struct SyntaxMap {
    version: clock::Global,
    snapshot: SyntaxSnapshot,
    language_registry: Option<Arc<LanguageRegistry>>,
}

#[derive(Clone, Default)]
pub struct SyntaxSnapshot {
    layers: SumTree<SyntaxLayer>,
}

#[derive(Clone)]
struct SyntaxLayer {
    depth: usize,
    range: Range<Anchor>,
    tree: tree_sitter::Tree,
    language: Arc<Language>,
}

#[derive(Debug, Clone)]
struct SyntaxLayerSummary {
    max_depth: usize,
    range: Range<Anchor>,
    last_layer_range: Range<Anchor>,
}

#[derive(Debug)]
struct DepthAndRange(usize, Range<Anchor>);

#[derive(Debug)]
struct DepthAndMaxPosition(usize, Anchor);

#[derive(Debug)]
struct DepthAndRangeOrMaxPosition(usize, Range<Anchor>, Anchor);

struct ReparseStep {
    depth: usize,
    language: Arc<Language>,
    ranges: Vec<tree_sitter::Range>,
    range: Range<Anchor>,
}

#[derive(Debug, PartialEq, Eq)]
struct ChangedRegion {
    depth: usize,
    range: Range<Anchor>,
}

impl SyntaxMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_language_registry(&mut self, registry: Arc<LanguageRegistry>) {
        self.language_registry = Some(registry);
    }

    pub fn snapshot(&self) -> SyntaxSnapshot {
        self.snapshot.clone()
    }

    pub fn interpolate(&mut self, text: &BufferSnapshot) {
        self.snapshot.interpolate(&self.version, text);
        self.version = text.version.clone();
    }

    pub fn reparse(&mut self, language: Arc<Language>, text: &BufferSnapshot) {
        self.version = text.version.clone();
        self.snapshot
            .reparse(self.language_registry.clone(), language, text);
    }
}

// Assumptions:
// * The maximum depth is small (< 5)
// * For a given depth, the number of layers that touch a given range
//   is small (usually only 1)

//                                  |change|
// 0 (............................................................)
// 1  (...............................................)
// 1                         (................)
// 1                                                    (.......)
// 2      (....)
// 2       (....)
// 2              (.......)
// 2                        (...)
// 2                               (.........)
// 2                                           (...)
// 3       (.)
// 3              (.)
// 3                  (..)
// 3                               (..)
// 3                                   (..)
// 3                                       (.)

impl SyntaxSnapshot {
    pub fn interpolate(&mut self, current_version: &clock::Global, text: &BufferSnapshot) {
        let edits = text
            .edits_since::<(usize, Point)>(&current_version)
            .collect::<Vec<_>>();
        if edits.is_empty() {
            return;
        }

        let mut layers = SumTree::new();
        let max_depth = self.layers.summary().max_depth;
        let mut cursor = self.layers.cursor::<SyntaxLayerSummary>();
        cursor.next(&text);

        for depth in 0..=max_depth {
            let mut edits = &edits[..];
            if cursor.start().max_depth < depth {
                layers.push_tree(
                    cursor.slice(
                        &DepthAndRange(depth, Anchor::MIN..Anchor::MAX),
                        Bias::Left,
                        text,
                    ),
                    text,
                );
            }

            while let Some(layer) = cursor.item() {
                let mut endpoints = text.summaries_for_anchors::<(usize, Point), _>([
                    &layer.range.start,
                    &layer.range.end,
                ]);
                let layer_range = endpoints.next().unwrap()..endpoints.next().unwrap();
                let start_byte = layer_range.start.0;
                let start_point = layer_range.start.1;

                // Preserve any layers at this depth that precede the first edit.
                let first_edit = if let Some(edit) = edits.first() {
                    edit
                } else {
                    break;
                };
                if first_edit.new.start.0 > layer_range.end.0 {
                    layers.push_tree(
                        cursor.slice(
                            &DepthAndMaxPosition(depth, text.anchor_before(first_edit.new.start.0)),
                            Bias::Left,
                            text,
                        ),
                        text,
                    );
                    continue;
                }

                // Preserve any layers at this depth that follow the last edit.
                let last_edit = edits.last().unwrap();
                if last_edit.new.end.0 < layer_range.start.0 {
                    break;
                }

                let mut layer = layer.clone();
                for (i, edit) in edits.iter().enumerate().rev() {
                    // Ignore any edits that start after the end of this layer.
                    if edit.new.start.0 > layer_range.end.0 {
                        continue;
                    }

                    // Ignore edits that end before the start of this layer, and don't consider them
                    // for any subsequent layers at this same depth.
                    if edit.new.end.0 <= start_byte {
                        edits = &edits[i + 1..];
                        break;
                    }

                    // Apply any edits that intersect this layer to the layer's syntax tree.
                    let tree_edit = if edit.new.start.0 >= start_byte {
                        tree_sitter::InputEdit {
                            start_byte: edit.new.start.0 - start_byte,
                            old_end_byte: edit.new.start.0 - start_byte
                                + (edit.old.end.0 - edit.old.start.0),
                            new_end_byte: edit.new.end.0 - start_byte,
                            start_position: (edit.new.start.1 - start_point).to_ts_point(),
                            old_end_position: (edit.new.start.1 - start_point
                                + (edit.old.end.1 - edit.old.start.1))
                                .to_ts_point(),
                            new_end_position: (edit.new.end.1 - start_point).to_ts_point(),
                        }
                    } else {
                        tree_sitter::InputEdit {
                            start_byte: 0,
                            old_end_byte: edit.new.end.0 - start_byte,
                            new_end_byte: 0,
                            start_position: Default::default(),
                            old_end_position: (edit.new.end.1 - start_point).to_ts_point(),
                            new_end_position: Default::default(),
                        }
                    };

                    layer.tree.edit(&tree_edit);
                    if edit.new.start.0 < start_byte {
                        break;
                    }
                }

                layers.push(layer, text);
                cursor.next(text);
            }
        }

        layers.push_tree(cursor.suffix(&text), &text);
        drop(cursor);
        self.layers = layers;
    }

    pub fn reparse(
        &mut self,
        registry: Option<Arc<LanguageRegistry>>,
        language: Arc<Language>,
        text: &BufferSnapshot,
    ) {
        let mut cursor = self.layers.cursor::<SyntaxLayerSummary>();
        cursor.next(&text);
        let mut layers = SumTree::new();

        let mut changed_regions = Vec::<ChangedRegion>::new();
        let mut queue = BinaryHeap::new();
        queue.push(ReparseStep {
            depth: 0,
            language: language.clone(),
            ranges: Vec::new(),
            range: Anchor::MIN..Anchor::MAX,
        });

        loop {
            let step = queue.pop();
            let (depth, range) = if let Some(step) = &step {
                (step.depth, step.range.clone())
            } else {
                (cursor.start().max_depth, Anchor::MAX..Anchor::MAX)
            };

            let target = DepthAndRange(depth, range.clone());
            if target.cmp(cursor.start(), &text).is_gt() {
                let change_start_anchor = changed_regions
                    .first()
                    .map_or(Anchor::MAX, |region| region.range.start);
                let seek_target =
                    DepthAndRangeOrMaxPosition(depth, range.clone(), change_start_anchor);
                let slice = cursor.slice(&seek_target, Bias::Left, text);
                layers.push_tree(slice, &text);

                while let Some(layer) = cursor.item() {
                    if target.cmp(&cursor.end(text), text).is_le() {
                        break;
                    }
                    if layer_is_changed(layer, text, &changed_regions) {
                        let region = ChangedRegion {
                            depth: depth + 1,
                            range: layer.range.clone(),
                        };
                        if let Err(i) =
                            changed_regions.binary_search_by(|probe| probe.cmp(&region, text))
                        {
                            changed_regions.insert(i, region);
                        }
                    } else {
                        layers.push(layer.clone(), text);
                    }

                    cursor.next(text);
                }

                changed_regions.retain(|region| {
                    region.depth > depth
                        || (region.depth == depth
                            && region.range.end.cmp(&range.start, text).is_gt())
                });
            }

            let (ranges, language) = if let Some(step) = step {
                (step.ranges, step.language)
            } else {
                break;
            };

            let start_point;
            let start_byte;
            let end_byte;
            if let Some((first, last)) = ranges.first().zip(ranges.last()) {
                start_point = first.start_point;
                start_byte = first.start_byte;
                end_byte = last.end_byte;
            } else {
                start_point = Point::zero().to_ts_point();
                start_byte = 0;
                end_byte = text.len();
            };

            let mut old_layer = cursor.item();
            if let Some(layer) = old_layer {
                if layer.range.to_offset(text) == (start_byte..end_byte) {
                    cursor.next(&text);
                } else {
                    old_layer = None;
                }
            }

            let grammar = if let Some(grammar) = language.grammar.as_deref() {
                grammar
            } else {
                continue;
            };

            let tree;
            let changed_ranges;
            if let Some(old_layer) = old_layer {
                tree = parse_text(
                    grammar,
                    text.as_rope(),
                    Some(old_layer.tree.clone()),
                    ranges,
                );

                changed_ranges = old_layer
                    .tree
                    .changed_ranges(&tree)
                    .map(|r| r.start_byte..r.end_byte)
                    .collect();
            } else {
                tree = parse_text(grammar, text.as_rope(), None, ranges);
                changed_ranges = vec![0..end_byte - start_byte];
            }

            layers.push(
                SyntaxLayer {
                    depth,
                    range,
                    tree: tree.clone(),
                    language: language.clone(),
                },
                &text,
            );

            if let (Some((config, registry)), false) = (
                grammar.injection_config.as_ref().zip(registry.as_ref()),
                changed_ranges.is_empty(),
            ) {
                let depth = depth + 1;

                for range in &changed_ranges {
                    let region = ChangedRegion {
                        depth,
                        range: text.anchor_before(range.start)..text.anchor_after(range.end),
                    };
                    if let Err(i) =
                        changed_regions.binary_search_by(|probe| probe.cmp(&region, text))
                    {
                        changed_regions.insert(i, region);
                    }
                }

                get_injections(
                    config,
                    text,
                    &tree,
                    registry,
                    depth,
                    start_byte,
                    Point::from_ts_point(start_point),
                    &changed_ranges,
                    &mut queue,
                );
            }
        }

        drop(cursor);
        self.layers = layers;
    }

    pub fn layers(&self, buffer: &BufferSnapshot) -> Vec<(&Grammar, &Tree, (usize, Point))> {
        self.layers
            .iter()
            .filter_map(|layer| {
                if let Some(grammar) = &layer.language.grammar {
                    Some((
                        grammar.as_ref(),
                        &layer.tree,
                        (
                            layer.range.start.to_offset(buffer),
                            layer.range.start.to_point(buffer),
                        ),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn layers_for_range<'a, T: ToOffset>(
        &self,
        range: Range<T>,
        buffer: &BufferSnapshot,
    ) -> Vec<(&Grammar, &Tree, (usize, Point))> {
        let start = buffer.anchor_before(range.start.to_offset(buffer));
        let end = buffer.anchor_after(range.end.to_offset(buffer));

        let mut cursor = self.layers.filter::<_, ()>(|summary| {
            let is_before_start = summary.range.end.cmp(&start, buffer).is_lt();
            let is_after_end = summary.range.start.cmp(&end, buffer).is_gt();
            !is_before_start && !is_after_end
        });

        let mut result = Vec::new();
        cursor.next(buffer);
        while let Some(layer) = cursor.item() {
            if let Some(grammar) = &layer.language.grammar {
                result.push((
                    grammar.as_ref(),
                    &layer.tree,
                    (
                        layer.range.start.to_offset(buffer),
                        layer.range.start.to_point(buffer),
                    ),
                ));
            }
            cursor.next(buffer)
        }

        result
    }
}

fn parse_text(
    grammar: &Grammar,
    text: &Rope,
    old_tree: Option<Tree>,
    mut ranges: Vec<tree_sitter::Range>,
) -> Tree {
    let (start_byte, start_point) = ranges
        .first()
        .map(|range| (range.start_byte, Point::from_ts_point(range.start_point)))
        .unwrap_or_default();

    for range in &mut ranges {
        range.start_byte -= start_byte;
        range.end_byte -= start_byte;
        range.start_point = (Point::from_ts_point(range.start_point) - start_point).to_ts_point();
        range.end_point = (Point::from_ts_point(range.end_point) - start_point).to_ts_point();
    }

    PARSER.with(|parser| {
        let mut parser = parser.borrow_mut();
        let mut chunks = text.chunks_in_range(start_byte..text.len());
        parser
            .set_included_ranges(&ranges)
            .expect("overlapping ranges");
        parser
            .set_language(grammar.ts_language)
            .expect("incompatible grammar");
        parser
            .parse_with(
                &mut move |offset, _| {
                    chunks.seek(start_byte + offset);
                    chunks.next().unwrap_or("").as_bytes()
                },
                old_tree.as_ref(),
            )
            .expect("invalid language")
    })
}

fn get_injections(
    config: &InjectionConfig,
    text: &BufferSnapshot,
    tree: &Tree,
    language_registry: &LanguageRegistry,
    depth: usize,
    start_byte: usize,
    start_point: Point,
    query_ranges: &[Range<usize>],
    queue: &mut BinaryHeap<ReparseStep>,
) -> bool {
    let mut result = false;
    let mut query_cursor = QueryCursorHandle::new();
    let mut prev_match = None;
    for query_range in query_ranges {
        query_cursor.set_byte_range(query_range.start..query_range.end);
        for mat in query_cursor.matches(
            &config.query,
            tree.root_node(),
            TextProvider(text.as_rope()),
        ) {
            let content_ranges = mat
                .nodes_for_capture_index(config.content_capture_ix)
                .map(|node| tree_sitter::Range {
                    start_byte: start_byte + node.start_byte(),
                    end_byte: start_byte + node.end_byte(),
                    start_point: (start_point + Point::from_ts_point(node.start_position()))
                        .to_ts_point(),
                    end_point: (start_point + Point::from_ts_point(node.end_position()))
                        .to_ts_point(),
                })
                .collect::<Vec<_>>();
            if content_ranges.is_empty() {
                continue;
            }

            // Avoid duplicate matches if two changed ranges intersect the same injection.
            let content_range =
                content_ranges.first().unwrap().start_byte..content_ranges.last().unwrap().end_byte;
            if let Some((last_pattern_ix, last_range)) = &prev_match {
                if mat.pattern_index == *last_pattern_ix && content_range == *last_range {
                    continue;
                }
            }
            prev_match = Some((mat.pattern_index, content_range.clone()));

            let language_name = config.languages_by_pattern_ix[mat.pattern_index]
                .as_ref()
                .map(|s| Cow::Borrowed(s.as_ref()))
                .or_else(|| {
                    let ix = config.language_capture_ix?;
                    let node = mat.nodes_for_capture_index(ix).next()?;
                    Some(Cow::Owned(
                        text.text_for_range(
                            start_byte + node.start_byte()..start_byte + node.end_byte(),
                        )
                        .collect(),
                    ))
                });

            if let Some(language_name) = language_name {
                if let Some(language) = language_registry.get_language(language_name.as_ref()) {
                    result = true;
                    let range = text.anchor_before(content_range.start)
                        ..text.anchor_after(content_range.end);
                    queue.push(ReparseStep {
                        depth,
                        language,
                        ranges: content_ranges,
                        range,
                    })
                }
            }
        }
    }
    result
}

fn layer_is_changed(
    layer: &SyntaxLayer,
    text: &BufferSnapshot,
    changed_regions: &[ChangedRegion],
) -> bool {
    changed_regions.iter().any(|region| {
        let is_before_layer = region.range.end.cmp(&layer.range.start, text).is_le();
        let is_after_layer = region.range.start.cmp(&layer.range.end, text).is_ge();
        !is_before_layer && !is_after_layer
    })
}

impl std::ops::Deref for SyntaxMap {
    type Target = SyntaxSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl PartialEq for ReparseStep {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

impl Eq for ReparseStep {}

impl PartialOrd for ReparseStep {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

impl Ord for ReparseStep {
    fn cmp(&self, other: &Self) -> Ordering {
        let range_a = self.range();
        let range_b = other.range();
        Ord::cmp(&other.depth, &self.depth)
            .then_with(|| Ord::cmp(&range_b.start, &range_a.start))
            .then_with(|| Ord::cmp(&range_a.end, &range_b.end))
    }
}

impl ReparseStep {
    fn range(&self) -> Range<usize> {
        let start = self.ranges.first().map_or(0, |r| r.start_byte);
        let end = self.ranges.last().map_or(0, |r| r.end_byte);
        start..end
    }
}

impl ChangedRegion {
    fn cmp(&self, other: &Self, buffer: &BufferSnapshot) -> Ordering {
        let range_a = &self.range;
        let range_b = &other.range;
        Ord::cmp(&self.depth, &other.depth)
            .then_with(|| range_a.start.cmp(&range_b.start, buffer))
            .then_with(|| range_b.end.cmp(&range_a.end, buffer))
    }
}

impl Default for SyntaxLayerSummary {
    fn default() -> Self {
        Self {
            max_depth: 0,
            range: Anchor::MAX..Anchor::MIN,
            last_layer_range: Anchor::MIN..Anchor::MAX,
        }
    }
}

impl sum_tree::Summary for SyntaxLayerSummary {
    type Context = BufferSnapshot;

    fn add_summary(&mut self, other: &Self, buffer: &Self::Context) {
        if other.max_depth > self.max_depth {
            *self = other.clone();
        } else {
            if other.range.start.cmp(&self.range.start, buffer).is_lt() {
                self.range.start = other.range.start;
            }
            if other.range.end.cmp(&self.range.end, buffer).is_gt() {
                self.range.end = other.range.end;
            }
            self.last_layer_range = other.last_layer_range.clone();
        }
    }
}

impl<'a> SeekTarget<'a, SyntaxLayerSummary, SyntaxLayerSummary> for DepthAndRange {
    fn cmp(&self, cursor_location: &SyntaxLayerSummary, buffer: &BufferSnapshot) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.max_depth)
            .then_with(|| {
                self.1
                    .start
                    .cmp(&cursor_location.last_layer_range.start, buffer)
            })
            .then_with(|| {
                cursor_location
                    .last_layer_range
                    .end
                    .cmp(&self.1.end, buffer)
            })
    }
}

impl<'a> SeekTarget<'a, SyntaxLayerSummary, SyntaxLayerSummary> for DepthAndMaxPosition {
    fn cmp(&self, cursor_location: &SyntaxLayerSummary, text: &BufferSnapshot) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.max_depth)
            .then_with(|| self.1.cmp(&cursor_location.range.end, text))
    }
}

impl<'a> SeekTarget<'a, SyntaxLayerSummary, SyntaxLayerSummary> for DepthAndRangeOrMaxPosition {
    fn cmp(&self, cursor_location: &SyntaxLayerSummary, buffer: &BufferSnapshot) -> Ordering {
        let cmp = Ord::cmp(&self.0, &cursor_location.max_depth);
        if cmp.is_ne() {
            return cmp;
        }

        let cmp = self.2.cmp(&cursor_location.range.end, buffer);
        if cmp.is_gt() {
            return Ordering::Greater;
        }

        self.1
            .start
            .cmp(&cursor_location.last_layer_range.start, buffer)
            .then_with(|| {
                cursor_location
                    .last_layer_range
                    .end
                    .cmp(&self.1.end, buffer)
            })
    }
}

impl sum_tree::Item for SyntaxLayer {
    type Summary = SyntaxLayerSummary;

    fn summary(&self) -> Self::Summary {
        SyntaxLayerSummary {
            max_depth: self.depth,
            range: self.range.clone(),
            last_layer_range: self.range.clone(),
        }
    }
}

impl std::fmt::Debug for SyntaxLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntaxLayer")
            .field("depth", &self.depth)
            .field("range", &self.range)
            .field("tree", &self.tree)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LanguageConfig;
    use text::{Buffer, Point};
    use tree_sitter::Query;
    use unindent::Unindent as _;
    use util::test::marked_text_ranges;

    #[gpui::test]
    fn test_syntax_map_layers_for_range() {
        let registry = Arc::new(LanguageRegistry::test());
        let language = Arc::new(rust_lang());
        registry.add(language.clone());

        let mut buffer = Buffer::new(
            0,
            0,
            r#"
                fn a() {
                    assert_eq!(
                        b(vec![C {}]),
                        vec![d.e],
                    );
                    println!("{}", f(|_| true));
                }
            "#
            .unindent(),
        );

        let mut syntax_map = SyntaxMap::new();
        syntax_map.set_language_registry(registry.clone());
        syntax_map.reparse(language.clone(), &buffer);

        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(2, 0)..Point::new(2, 0),
            &[
                "...(function_item ... (block (expression_statement (macro_invocation...",
                "...(tuple_expression (call_expression ... arguments: (arguments (macro_invocation...",
            ],
        );
        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(2, 14)..Point::new(2, 16),
            &[
                "...(function_item ...",
                "...(tuple_expression (call_expression ... arguments: (arguments (macro_invocation...",
                "...(array_expression (struct_expression ...",
            ],
        );
        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(3, 14)..Point::new(3, 16),
            &[
                "...(function_item ...",
                "...(tuple_expression (call_expression ... arguments: (arguments (macro_invocation...",
                "...(array_expression (field_expression ...",
            ],
        );
        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(5, 12)..Point::new(5, 16),
            &[
                "...(function_item ...",
                "...(call_expression ... (arguments (closure_expression ...",
            ],
        );

        // Replace a vec! macro invocation with a plain slice, removing a syntactic layer.
        let macro_name_range = range_for_text(&buffer, "vec!");
        buffer.edit([(macro_name_range, "&")]);
        syntax_map.interpolate(&buffer);
        syntax_map.reparse(language.clone(), &buffer);

        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(2, 14)..Point::new(2, 16),
            &[
                "...(function_item ...",
                "...(tuple_expression (call_expression ... arguments: (arguments (reference_expression value: (array_expression...",
            ],
        );

        // Put the vec! macro back, adding back the syntactic layer.
        buffer.undo();
        syntax_map.interpolate(&buffer);
        syntax_map.reparse(language.clone(), &buffer);

        assert_layers_for_range(
            &syntax_map,
            &buffer,
            Point::new(2, 14)..Point::new(2, 16),
            &[
                "...(function_item ...",
                "...(tuple_expression (call_expression ... arguments: (arguments (macro_invocation...",
                "...(array_expression (struct_expression ...",
            ],
        );
    }

    #[gpui::test]
    fn test_syntax_map_edits() {
        let registry = Arc::new(LanguageRegistry::test());
        let language = Arc::new(rust_lang());
        let mut syntax_map = SyntaxMap::new();
        syntax_map.set_language_registry(registry.clone());
        registry.add(language.clone());

        let mut buffer = Buffer::new(0, 0, "".into());
        syntax_map.reparse(language.clone(), &buffer);

        edit_buffer_n(
            &mut buffer,
            &[
                "«fn a() { dbg }»",
                "fn a() { dbg«!» }",
                "fn a() { dbg!«()» }",
                "fn a() { dbg!(«b») }",
                "fn a() { dbg!(b«.») }",
                "fn a() { dbg!(b.«c») }",
                "fn a() { dbg!(b.c«()») }",
                "fn a() { dbg!(b.c(«vec»)) }",
                "fn a() { dbg!(b.c(vec«!»)) }",
                "fn a() { dbg!(b.c(vec!«[]»)) }",
                "fn a() { dbg!(b.c(vec![«d»])) }",
                "fn a() { dbg!(b.c(vec![d«.»])) }",
                "fn a() { dbg!(b.c(vec![d.«e»])) }",
            ],
        );

        syntax_map.interpolate(&buffer);
        syntax_map.reparse(language.clone(), &buffer);

        assert_node_ranges(
            &syntax_map,
            &buffer,
            "(field_identifier) @_",
            "fn a() { dbg!(b.«c»(vec![d.«e»])) }",
        );
    }

    fn rust_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Rust".into(),
                path_suffixes: vec!["rs".to_string()],
                ..Default::default()
            },
            Some(tree_sitter_rust::language()),
        )
        .with_injection_query(
            r#"
                (macro_invocation
                    (token_tree) @content
                    (#set! "language" "rust"))
            "#,
        )
        .unwrap()
    }

    fn range_for_text(buffer: &Buffer, text: &str) -> Range<usize> {
        let start = buffer.as_rope().to_string().find(text).unwrap();
        start..start + text.len()
    }

    fn assert_layers_for_range(
        syntax_map: &SyntaxMap,
        buffer: &BufferSnapshot,
        range: Range<Point>,
        expected_layers: &[&str],
    ) {
        let layers = syntax_map.layers_for_range(range, &buffer);
        assert_eq!(
            layers.len(),
            expected_layers.len(),
            "wrong number of layers"
        );
        for (i, ((_, tree, _), expected_s_exp)) in
            layers.iter().zip(expected_layers.iter()).enumerate()
        {
            let actual_s_exp = tree.root_node().to_sexp();
            assert!(
                string_contains_sequence(
                    &actual_s_exp,
                    &expected_s_exp.split("...").collect::<Vec<_>>()
                ),
                "layer {i}:\n\nexpected: {expected_s_exp}\nactual:   {actual_s_exp}",
            );
        }
    }

    fn assert_node_ranges(
        syntax_map: &SyntaxMap,
        buffer: &BufferSnapshot,
        query: &str,
        marked_string: &str,
    ) {
        let mut cursor = QueryCursorHandle::new();
        let mut actual_ranges = Vec::<Range<usize>>::new();
        for (grammar, tree, (start_byte, _)) in syntax_map.layers(buffer) {
            let query = Query::new(grammar.ts_language, query).unwrap();
            for (mat, ix) in
                cursor.captures(&query, tree.root_node(), TextProvider(buffer.as_rope()))
            {
                let range = mat.captures[ix].node.byte_range();
                actual_ranges.push(start_byte + range.start..start_byte + range.end);
            }
        }

        let (text, expected_ranges) = marked_text_ranges(marked_string, false);
        assert_eq!(text, buffer.text());
        assert_eq!(actual_ranges, expected_ranges);
    }

    fn edit_buffer_n(buffer: &mut Buffer, marked_strings: &[&str]) {
        for marked_string in marked_strings {
            edit_buffer(buffer, marked_string);
        }
    }

    fn edit_buffer(buffer: &mut Buffer, marked_string: &str) {
        let old_text = buffer.text();
        let (new_text, mut ranges) = marked_text_ranges(marked_string, false);
        assert_eq!(ranges.len(), 1);

        let inserted_range = ranges.pop().unwrap();
        let inserted_text = new_text[inserted_range.clone()].to_string();
        let deleted_len = (inserted_range.len() as isize + old_text.len() as isize
            - new_text.len() as isize) as usize;
        let deleted_range = inserted_range.start..inserted_range.start + deleted_len;

        assert_eq!(
            old_text[..deleted_range.start],
            new_text[..inserted_range.start],
            "invalid edit",
        );
        assert_eq!(
            old_text[deleted_range.end..],
            new_text[inserted_range.end..],
            "invalid edit",
        );

        buffer.edit([(deleted_range, inserted_text)]);
    }

    pub fn string_contains_sequence(text: &str, parts: &[&str]) -> bool {
        let mut last_part_end = 0;
        for part in parts {
            if let Some(start_ix) = text[last_part_end..].find(part) {
                last_part_end = start_ix + part.len();
            } else {
                return false;
            }
        }
        true
    }
}
