//! Generates a [FeaRsMarks] datastructure to be fed to fea-rs

use std::collections::{BTreeMap, BTreeSet, HashSet};

use fea_rs::{
    compile::{
        FeatureProvider, MarkToBaseBuilder, MarkToMarkBuilder, NopFeatureProvider,
        NopVariationInfo, PendingLookup,
    },
    typed::{AstNode, LanguageSystem},
    GlyphSet, Opts, ParseTree,
};
use fontdrasil::{
    orchestration::{Access, AccessBuilder, Work},
    types::GlyphName,
};

use ordered_float::OrderedFloat;
use smol_str::SmolStr;
use write_fonts::{
    tables::{gdef::GlyphClassDef, layout::LookupFlag},
    types::{GlyphId, Tag},
};

use crate::{
    error::Error,
    orchestration::{AnyWorkId, BeWork, Context, FeaRsMarks, WorkId},
};
use fontir::{
    ir::{self, Anchor, AnchorKind, GlyphAnchors, GlyphOrder, StaticMetadata},
    orchestration::WorkId as FeWorkId,
};

use super::DFLT_SCRIPT;

#[derive(Debug)]
struct MarkWork {}

pub fn create_mark_work() -> Box<BeWork> {
    Box::new(MarkWork {})
}

/// The canonical name shared for a given mark/base pair, e.g. `top` for `top`/`_top`
type MarkGroupName = SmolStr;

struct MarkLookupBuilder<'a> {
    // extracted from public.openTypeCatgories/GlyphData.xml or FEA
    gdef_classes: BTreeMap<GlyphName, GlyphClassDef>,
    // pruned, only the anchors we are using
    anchor_lists: BTreeMap<GlyphName, Vec<&'a ir::Anchor>>,
    glyph_order: &'a GlyphOrder,
    static_metadata: &'a StaticMetadata,
    // we don't currently use this, because just adding lookups to all scripts works?
    _fea_scripts: HashSet<Tag>,
    mark_glyphs: BTreeSet<GlyphName>,
}

/// The bases and marks in a particular group, e.g. "top" or "bottom"
#[derive(Default, Debug, Clone, PartialEq)]
struct MarkGroup<'a> {
    bases: Vec<(GlyphName, &'a ir::Anchor)>,
    marks: Vec<(GlyphName, &'a ir::Anchor)>,
    // if `true`, we will make a mark filter set from the marks in this group
    // (only true for mkmk)
    filter_glyphs: bool,
}

impl MarkGroup<'_> {
    fn make_filter_glyph_set(&self, glyph_order: &GlyphOrder) -> Option<GlyphSet> {
        self.filter_glyphs.then(|| {
            self.marks
                .iter()
                .map(|(name, _)| glyph_order.glyph_id(name).unwrap())
                .collect()
        })
    }
}

// a trait to abstract over two very similar builders
trait MarkAttachmentBuilder: Default {
    fn add_mark(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor);
    fn add_base(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor);
}

impl MarkAttachmentBuilder for MarkToBaseBuilder {
    fn add_mark(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor) {
        //FIXME: precheck groups to ensure no overlap
        let _ = self.insert_mark(gid, group.clone(), anchor);
    }

    fn add_base(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor) {
        self.insert_base(gid, group, anchor)
    }
}

impl MarkAttachmentBuilder for MarkToMarkBuilder {
    fn add_mark(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor) {
        //FIXME: precheck groups to ensure no overlap
        let _ = self.insert_mark1(gid, group.clone(), anchor);
    }

    fn add_base(&mut self, gid: GlyphId, group: &MarkGroupName, anchor: fea_rs::compile::Anchor) {
        self.insert_mark2(gid, group, anchor)
    }
}

impl<'a> MarkLookupBuilder<'a> {
    fn new(
        anchors: Vec<&'a GlyphAnchors>,
        glyph_order: &'a GlyphOrder,
        static_metadata: &'a StaticMetadata,
        ast: &ParseTree,
    ) -> Self {
        let gdef_classes = get_gdef_classes(static_metadata, ast, glyph_order);
        let fea_scripts = get_fea_scripts(ast);
        // first we want to narrow our input down to only anchors that are participating.
        // in pythonland this is https://github.com/googlefonts/ufo2ft/blob/8e9e6eb66a/Lib/ufo2ft/featureWriters/markFeatureWriter.py#L380
        let mut pruned = BTreeMap::new();
        let mut base_groups = HashSet::new();
        let mut mark_groups = HashSet::new();
        let include = gdef_classes
            .iter()
            .filter_map(|(name, cls)| {
                matches!(
                    *cls,
                    GlyphClassDef::Base | GlyphClassDef::Mark | GlyphClassDef::Ligature
                )
                .then_some(name)
            })
            .collect::<HashSet<_>>();
        for anchors in anchors {
            // skip anchors for non-export glyphs
            if !glyph_order.contains(&anchors.glyph_name) {
                continue;
            }
            // skip glyphs that are not mark/lig/base, if we have any defined categories
            if !include.is_empty() && !include.contains(&anchors.glyph_name) {
                continue;
            }
            for anchor in &anchors.anchors {
                match &anchor.kind {
                    AnchorKind::Base(group)
                    | AnchorKind::Ligature {
                        group_name: group, ..
                    } => {
                        base_groups.insert(group);
                    }
                    AnchorKind::Mark(group) => {
                        mark_groups.insert(group);
                    }
                    // skip non base/mark anchors
                    _ => continue,
                }
                pruned
                    .entry(anchors.glyph_name.clone())
                    .or_insert(Vec::new())
                    .push(anchor);
            }
        }

        let used_groups = base_groups
            .intersection(&mark_groups)
            .collect::<HashSet<_>>();
        // we don't care about marks with no corresponding bases, & vice-versa see:
        // <https://github.com/googlefonts/ufo2ft/blob/6787e37e63530/Lib/ufo2ft/featureWriters/markFeatureWriter.py#L359>
        pruned.retain(|_, anchors| {
            anchors.retain(|anchor| {
                anchor
                    .mark_group_name()
                    .map(|group| used_groups.contains(&group))
                    .unwrap_or(false)
            });
            !anchors.is_empty()
        });

        let mark_glyphs = find_mark_glyphs(&pruned, &gdef_classes);
        Self {
            anchor_lists: pruned,
            glyph_order,
            _fea_scripts: fea_scripts,
            static_metadata,
            gdef_classes,
            mark_glyphs,
        }
    }

    fn build(&self) -> Result<FeaRsMarks, Error> {
        let mark_base_groups = self.make_mark_to_base_groups();
        let mark_mark_groups = self.make_mark_to_mark_groups();

        let mark_base = self.make_lookups::<MarkToBaseBuilder>(mark_base_groups)?;
        let mark_mark = self.make_lookups::<MarkToMarkBuilder>(mark_mark_groups)?;
        Ok(FeaRsMarks {
            glyphmap: self.glyph_order.iter().cloned().collect(),
            mark_base,
            mark_mark,
        })
    }

    fn make_lookups<T: MarkAttachmentBuilder>(
        &self,
        groups: BTreeMap<MarkGroupName, MarkGroup>,
    ) -> Result<Vec<PendingLookup<T>>, Error> {
        groups
            .into_iter()
            .filter(|(_, group)| !(group.bases.is_empty() || group.marks.is_empty()))
            .map(|(group_name, group)| {
                let mut builder = T::default();
                let filter_set = group.make_filter_glyph_set(self.glyph_order);
                let mut flags = LookupFlag::empty();
                flags.set_use_mark_filtering_set(filter_set.is_some());
                for (mark_name, anchor) in group.marks {
                    // we already filtered to only things in glyph order
                    let gid = self.glyph_order.glyph_id(&mark_name).unwrap();
                    let anchor = resolve_anchor(anchor, self.static_metadata, &mark_name)?;
                    builder.add_mark(gid, &group_name, anchor);
                }

                for (base_name, anchor) in group.bases {
                    let gid = self.glyph_order.glyph_id(&base_name).unwrap();
                    let anchor = resolve_anchor(anchor, self.static_metadata, &base_name)?;
                    builder.add_base(gid, &group_name, anchor);
                }
                Ok(PendingLookup::new(vec![builder], flags, filter_set))
            })
            .collect()
    }

    fn make_mark_to_base_groups(&self) -> BTreeMap<MarkGroupName, MarkGroup<'a>> {
        let mut groups = BTreeMap::<_, MarkGroup>::new();
        for (glyph_name, anchors) in &self.anchor_lists {
            let is_mark = self.mark_glyphs.contains(glyph_name);
            // if we have explicit gdef classes and this is not an expilcit base
            let is_not_base = !self.gdef_classes.is_empty()
                && (self.gdef_classes.get(glyph_name)) != Some(&GlyphClassDef::Base);

            let treat_as_base = !(is_mark | is_not_base);
            for anchor in anchors {
                match &anchor.kind {
                    fontir::ir::AnchorKind::Base(group) if treat_as_base => groups
                        .entry(group.clone())
                        .or_default()
                        .bases
                        .push((glyph_name.clone(), anchor)),
                    fontir::ir::AnchorKind::Mark(group) if is_mark => groups
                        .entry(group.clone())
                        .or_default()
                        .marks
                        .push((glyph_name.clone(), anchor)),
                    _ => continue,
                }
            }
        }
        groups
    }

    fn make_mark_to_mark_groups(&self) -> BTreeMap<MarkGroupName, MarkGroup<'a>> {
        // first find the set of glyphs that are marks, i.e. have any mark attachment point.
        let (mark_glyphs, mark_anchors): (BTreeSet<_>, BTreeSet<_>) = self
            .anchor_lists
            .iter()
            .filter(|(name, _)| self.mark_glyphs.contains(*name))
            .flat_map(|(name, anchors)| {
                anchors.iter().filter_map(move |anchor| {
                    if let AnchorKind::Mark(group) = &anchor.kind {
                        Some((name, group))
                    } else {
                        None
                    }
                })
            })
            .unzip();

        // then iterate again, looking for glyphs that we have identified as marks,
        // but which also have participating base anchors
        let mut result = BTreeMap::<MarkGroupName, MarkGroup>::new();
        for (glyph, glyph_anchors) in &self.anchor_lists {
            if !mark_glyphs.contains(glyph) {
                continue;
            }

            for anchor in glyph_anchors {
                if let AnchorKind::Base(group_name) = &anchor.kind {
                    // only if this anchor is a base, AND we have a mark in the same group
                    if mark_anchors.contains(group_name) {
                        let group = result.entry(group_name.clone()).or_default();
                        group.filter_glyphs = true;
                        group.bases.push((glyph.clone(), anchor))
                    }
                }
            }
        }

        // then add the anchors for the mark glyphs, if they exist
        for mark in mark_glyphs {
            let anchors = self.anchor_lists.get(mark).unwrap();
            for anchor in anchors {
                if let AnchorKind::Mark(group) = &anchor.kind {
                    // only add mark glyph if there is at least one base
                    if let Some(group) = result.get_mut(group) {
                        group.marks.push((mark.clone(), anchor))
                    }
                }
            }
        }
        result
    }
}

impl Work<Context, AnyWorkId, Error> for MarkWork {
    fn id(&self) -> AnyWorkId {
        WorkId::Marks.into()
    }

    fn read_access(&self) -> Access<AnyWorkId> {
        AccessBuilder::new()
            .variant(FeWorkId::StaticMetadata)
            .variant(FeWorkId::GlyphOrder)
            .variant(WorkId::FeaturesAst)
            .variant(FeWorkId::ALL_ANCHORS)
            .build()
    }

    /// Generate mark data structures.
    fn exec(&self, context: &Context) -> Result<(), Error> {
        let static_metadata = context.ir.static_metadata.get();
        let glyph_order = context.ir.glyph_order.get();
        let raw_anchors = context.ir.anchors.all();
        let ast = context.fea_ast.get();

        let anchors = raw_anchors
            .iter()
            .map(|(_, anchors)| anchors.as_ref())
            .collect::<Vec<_>>();

        // this code is roughly equivalent to what in pythonland happens in
        // setContext: https://github.com/googlefonts/ufo2ft/blob/8e9e6eb66a/Lib/ufo2ft/featureWriters/markFeatureWriter.py#L322
        let ctx = MarkLookupBuilder::new(anchors, &glyph_order, &static_metadata, &ast.ast);
        let all_marks = ctx.build()?;

        context.fea_rs_marks.set(all_marks);

        Ok(())
    }
}

// in py this is set during _groupMarkGlyphsByAnchor; we try to match that logic
// https://github.com/googlefonts/ufo2ft/blob/8e9e6eb66/Lib/ufo2ft/featureWriters/markFeatureWriter.py#L412
fn find_mark_glyphs(
    anchors: &BTreeMap<GlyphName, Vec<&Anchor>>,
    gdef_classes: &BTreeMap<GlyphName, GlyphClassDef>,
) -> BTreeSet<GlyphName> {
    anchors
        .iter()
        .filter(|(name, anchors)| {
            (!gdef_classes.is_empty() && gdef_classes.get(*name) == Some(&GlyphClassDef::Mark))
                && anchors.iter().any(|a| a.is_mark())
        })
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn get_fea_scripts(ast: &ParseTree) -> HashSet<Tag> {
    ast.typed_root()
        .statements()
        .filter_map(LanguageSystem::cast)
        .map(|langsys| langsys.script().to_raw())
        .filter(|tag| *tag != DFLT_SCRIPT)
        .collect()
}

fn get_gdef_classes(
    meta: &StaticMetadata,
    ast: &ParseTree,
    glyph_order: &GlyphOrder,
) -> BTreeMap<GlyphName, GlyphClassDef> {
    let glyph_map = glyph_order.iter().cloned().collect();
    // if we prefer classes defined in fea, compile the fea and see if we have any
    if meta.gdef_categories.prefer_gdef_categories_in_fea {
        if let Some(gdef_classes) =
            fea_rs::compile::compile::<NopVariationInfo, NopFeatureProvider>(
                ast,
                &glyph_map,
                None,
                None,
                Opts::new().compile_gpos(false),
            )
            .ok()
            .and_then(|mut comp| comp.0.gdef_classes.take())
        {
            return gdef_classes
                .into_iter()
                .filter_map(|(g, cls)| {
                    glyph_order
                        .glyph_name(g.to_u16() as _)
                        .cloned()
                        .map(|name| (name, cls))
                })
                .collect();
        }
    }
    // otherwise (we don't care about FEA or nothing is defined) we use the
    // classes in StaticMetadata (which are from public.openTypeCategories or
    // GlyphData.xml)
    meta.gdef_categories.categories.clone()
}

fn resolve_anchor(
    anchor: &fontir::ir::Anchor,
    static_metadata: &StaticMetadata,
    glyph_name: &GlyphName, // just used for error reporting
) -> Result<fea_rs::compile::Anchor, Error> {
    let (x_values, y_values): (Vec<_>, Vec<_>) = anchor
        .positions
        .iter()
        .map(|(loc, pt)| {
            (
                (loc.clone(), OrderedFloat::from(pt.x as f32)),
                (loc.clone(), OrderedFloat::from(pt.y as f32)),
            )
        })
        .unzip();

    let (x_default, x_deltas) = crate::features::resolve_variable_metric(
        static_metadata,
        // If I do map(|(pos, value)| (pos, value)) to destructure the tuple and
        // convert &(T, V) => (&T, &V) as the values parameter expects, then
        // clippy complains about the seemingly no-op identity map:
        // https://rust-lang.github.io/rust-clippy/master/index.html#/map_identity
        x_values.iter().map(|item| (&item.0, &item.1)),
    )
    .map_err(|err| Error::AnchorDeltaError(glyph_name.to_owned(), err))?;
    let (y_default, y_deltas) = crate::features::resolve_variable_metric(
        static_metadata,
        y_values.iter().map(|item| (&item.0, &item.1)),
    )
    .map_err(|err| Error::AnchorDeltaError(glyph_name.to_owned(), err))?;

    let mut anchor = fea_rs::compile::Anchor::new(x_default, y_default);
    if x_deltas.iter().any(|v| v.1 != 0) {
        anchor = anchor.with_x_device(x_deltas);
    }
    if y_deltas.iter().any(|v| v.1 != 0) {
        anchor = anchor.with_y_device(y_deltas);
    }

    Ok(anchor)
}

impl FeatureProvider for FeaRsMarks {
    fn add_features(&self, builder: &mut fea_rs::compile::FeatureBuilder) {
        let mut mark_base_lookups = Vec::new();
        let mut mark_mark_lookups = Vec::new();

        for mark_base in self.mark_base.iter() {
            // each mark to base it's own lookup, whch differs from fontmake
            mark_base_lookups.push(builder.add_lookup(mark_base.clone()));
        }

        // If a mark has anchors that are themselves marks what we got here is a mark to mark
        for mark_mark in self.mark_mark.iter() {
            mark_mark_lookups.push(builder.add_lookup(mark_mark.clone()));
        }

        if !mark_base_lookups.is_empty() {
            builder.add_to_default_language_systems(Tag::new(b"mark"), &mark_base_lookups);
        }
        if !mark_mark_lookups.is_empty() {
            builder.add_to_default_language_systems(Tag::new(b"mkmk"), &mark_mark_lookups);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, sync::Arc};

    use fea_rs::compile::Compilation;
    use fontdrasil::{
        coords::{Coord, CoordConverter, NormalizedLocation},
        types::Axis,
    };
    use fontir::ir::{GdefCategories, NamedInstance};
    use kurbo::Point;

    use write_fonts::{
        dump_table,
        read::{
            tables::{gdef::Gdef as RGdef, gpos::Gpos as RGpos},
            FontRead,
        },
    };

    use crate::features::FeaVariationInfo;

    use super::*;

    /// A helper for testing our mark generation code
    #[derive(Clone, Debug)]
    struct MarksInput<const N: usize> {
        locations: [NormalizedLocation; N],
        anchors: BTreeMap<GlyphName, Vec<Anchor>>,
        categories: BTreeMap<GlyphName, GlyphClassDef>,
        user_fea: Arc<str>,
    }

    struct AnchorBuilder<const N: usize> {
        locations: [NormalizedLocation; N],
        anchors: Vec<Anchor>,
    }

    impl Default for MarksInput<1> {
        fn default() -> Self {
            Self::new([&[0.0]])
        }
    }

    impl<const N: usize> AnchorBuilder<N> {
        /// Add a new anchor, with positions defined for each of our locations
        ///
        /// The 'name' should be a raw anchor name, and should be known-good.
        /// Anchor name tests are in `fontir`.
        fn add(&mut self, anchor_name: &str, pos: [(i16, i16); N]) -> &mut Self {
            let positions = pos
                .into_iter()
                .enumerate()
                .map(|(i, pt)| (self.locations[i].clone(), Point::new(pt.0 as _, pt.1 as _)))
                .collect();
            let kind = AnchorKind::new(anchor_name).unwrap();
            self.anchors.push(Anchor { kind, positions });
            self
        }
    }

    impl<const N: usize> MarksInput<N> {
        /// Create test input with `N` locations
        fn new(locations: [&[f32]; N]) -> Self {
            const TAG_NAMES: [Tag; 3] = [Tag::new(b"axs1"), Tag::new(b"axs2"), Tag::new(b"axs3")];
            let locations = locations
                .iter()
                .map(|loc| {
                    loc.iter()
                        .enumerate()
                        .map(|(i, pos)| {
                            (
                                *TAG_NAMES.get(i).expect("too many axes in test"),
                                Coord::new(*pos),
                            )
                        })
                        .collect()
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            Self {
                user_fea: "languagesystem DFLT dflt;".into(),
                locations,
                anchors: Default::default(),
                categories: Default::default(),
            }
        }

        /// Provide custom user FEA.
        ///
        /// By default we use a single 'languagesytem DFLT dflt' statement.
        fn set_user_fea(&mut self, fea: &str) -> &mut Self {
            self.user_fea = fea.into();
            self
        }

        /// Add a glyph with an optional GDEF category.
        ///
        /// the `anchors_fn` argument is a closure where anchors can be added
        /// to the newly added glyph.
        fn add_glyph(
            &mut self,
            name: &str,
            category: impl Into<Option<GlyphClassDef>>,
            mut anchors_fn: impl FnMut(&mut AnchorBuilder<N>),
        ) -> &mut Self {
            let name: GlyphName = name.into();
            if let Some(category) = category.into() {
                self.categories.insert(name.clone(), category);
            }
            let mut anchors = AnchorBuilder {
                locations: self.locations.clone(),
                anchors: Default::default(),
            };

            anchors_fn(&mut anchors);
            self.anchors.insert(name, anchors.anchors);
            self
        }

        fn make_static_metadata(&self) -> StaticMetadata {
            let (min, default, max) = (Coord::new(-1.), Coord::new(0.0), Coord::new(1.0));
            let axes = self.locations[0]
                .axis_tags()
                .map(|tag| Axis {
                    name: tag.to_string(),
                    tag: *tag,
                    min,
                    default,
                    max,
                    hidden: false,
                    converter: CoordConverter::unmapped(min, default, max),
                })
                .collect::<Vec<_>>();
            let axis_map = axes.iter().map(|a| (a.tag, a)).collect();
            let named_instances = self
                .locations
                .iter()
                .enumerate()
                .map(|(i, loc)| NamedInstance {
                    name: format!("instance{i}"),
                    location: loc.to_user(&axis_map),
                })
                .collect();
            let glyph_locations = self.locations.iter().cloned().collect();
            let categories = GdefCategories {
                categories: self.categories.clone(),
                prefer_gdef_categories_in_fea: false,
            };
            StaticMetadata::new(
                1000,
                Default::default(),
                axes,
                named_instances,
                glyph_locations,
                Default::default(),
                42.,
                categories,
            )
            .unwrap()
        }

        fn run_marks_writer(
            &self,
            static_metadata: &StaticMetadata,
            ast: &ParseTree,
        ) -> FeaRsMarks {
            let anchors = self
                .anchors
                .iter()
                .map(|(name, anchors)| GlyphAnchors::new(name.clone(), anchors.clone()).unwrap())
                .collect::<Vec<_>>();
            let anchorsref = anchors.iter().collect();
            let glyph_order = self.anchors.keys().cloned().collect();

            let ctx = MarkLookupBuilder::new(anchorsref, &glyph_order, static_metadata, ast);

            ctx.build().unwrap()
        }

        fn compile(&self) -> Compilation {
            let static_metadata = self.make_static_metadata();
            let fea = self.user_fea.clone();
            let glyph_map = self.anchors.keys().cloned().collect();
            // first get the AST, which we need to use as input
            let (ast, _) = fea_rs::parse::parse_root(
                "memory".into(),
                Some(&glyph_map),
                Box::new(move |x: &OsStr| {
                    if x == "memory" {
                        Ok(fea.clone())
                    } else {
                        unreachable!("our FEA has no include statements");
                    }
                }),
            )
            .unwrap();

            // then run the marks code in this module:
            let marks = self.run_marks_writer(&static_metadata, &ast);
            let var_info = FeaVariationInfo::new(&static_metadata);
            // then compile with fea-rs, passing in our generated marks:
            let (result, _) = fea_rs::compile::compile(
                &ast,
                &glyph_map,
                Some(&var_info),
                Some(&marks),
                Default::default(),
            )
            .unwrap();
            result
        }

        /// Build the GPOS & GDEF tables and get a textual representation
        fn get_normalized_output(&mut self) -> String {
            let result = self.compile();
            // okay, so now we have some write-fonts tables; convert to read-fonts
            let Some(gpos) = result.gpos else {
                return String::new();
            };
            let gpos_bytes = write_fonts::dump_table(&gpos).unwrap();
            let gdef_bytes = result.gdef.map(|gdef| dump_table(&gdef).unwrap());
            let gpos = RGpos::read(gpos_bytes.as_slice().into()).unwrap();
            let gdef = gdef_bytes
                .as_ref()
                .map(|b| RGdef::read(b.as_slice().into()).unwrap());
            let mut buf = Vec::new();
            let names = self.anchors.keys().cloned().collect();

            // and pass these to layout normalizer
            layout_normalizer::print_gpos(&mut buf, &gpos, gdef.as_ref(), &names).unwrap();
            String::from_utf8(buf).unwrap()
        }
    }

    // does some cleanup so that we don't need to worry about indentation when comparing strings
    fn normalize_layout_repr(s: &str) -> String {
        s.trim().chars().filter(|c| *c != ' ').collect()
    }
    macro_rules! assert_eq_ignoring_ws {
        ($left:expr, $right:expr) => {
            let left = normalize_layout_repr(&$left);
            let right = normalize_layout_repr(&$right);
            pretty_assertions::assert_str_eq!(left, right)
        };
    }

    // sanity check that if we don't make empty lookups
    #[test]
    fn no_anchors_no_feature() {
        let out = MarksInput::default()
            .add_glyph("a", None, |_| {})
            .add_glyph("acutecomb", None, |_| {})
            .get_normalized_output();

        assert!(out.is_empty());
    }

    #[test]
    fn attach_a_mark_to_a_base() {
        let out = MarksInput::default()
            .add_glyph("A", GlyphClassDef::Base, |anchors| {
                anchors.add("top", [(100, 400)]);
            })
            .add_glyph("acutecomb", GlyphClassDef::Mark, |anchors| {
                anchors.add("_top", [(50, 50)]);
            })
            .get_normalized_output();
        assert_eq_ignoring_ws!(
            out,
            r#"
            # mark: DFLT/dflt ## 1 MarkToBase rules
            # lookupflag LookupFlag(0)
            A @(x: 100, y: 400)
              @(x: 50, y: 50) acutecomb
            "#
        );
    }

    #[test]
    fn custom_fea() {
        let out = MarksInput::default()
            .set_user_fea("languagesystem latn dflt;")
            .add_glyph("A", GlyphClassDef::Base, |anchors| {
                anchors.add("top", [(100, 400)]);
            })
            .add_glyph("acutecomb", GlyphClassDef::Mark, |anchors| {
                anchors.add("_top", [(50, 50)]);
            })
            .get_normalized_output();
        assert_eq_ignoring_ws!(
            out,
            r#"
            # mark: latn/dflt ## 1 MarkToBase rules
            # lookupflag LookupFlag(0)
            A @(x: 100, y: 400)
              @(x: 50, y: 50) acutecomb
            "#
        );
    }
}
