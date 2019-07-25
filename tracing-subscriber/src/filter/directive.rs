use super::{
    field,
    level::{self, LevelFilter},
    FieldMap, FilterVec,
};
use lazy_static::lazy_static;
use regex::Regex;
use std::{
    cmp::Ordering, collections::BTreeSet, error::Error, fmt, iter::FromIterator, str::FromStr,
};
use tracing_core::{span, Metadata};

/// A single filtering directive.
// TODO(eliza): add a builder for programmatically constructing directives?
#[derive(Debug, Eq, PartialEq)]
pub struct Directive {
    target: Option<String>,
    in_span: Option<String>,
    fields: FilterVec<field::Match>,
    level: LevelFilter,
}

/// A directive which will statically enable or disable a given callsite.
///
/// Unlike a dynamic directive, this can be cached by the callsite.
#[derive(Debug, PartialEq, Eq, Ord)]
pub struct StaticDirective {
    target: Option<String>,
    level: LevelFilter,
}

pub trait Match {
    fn cares_about(&self, meta: &Metadata) -> bool;
    fn level(&self) -> &LevelFilter;
}

/// A set of dynamic filtering directives.
pub type Dynamics = DirectiveSet<Directive>;

/// A set of static filtering directives.
pub type Statics = DirectiveSet<StaticDirective>;

#[derive(Debug)]
pub struct DirectiveSet<T> {
    directives: BTreeSet<T>,
    max_level: LevelFilter,
}

pub type CallsiteMatcher = MatchSet<field::CallsiteMatch>;
pub type SpanMatcher = MatchSet<field::SpanMatch>;

#[derive(Debug, PartialEq, Eq)]
pub struct MatchSet<T> {
    field_matches: FilterVec<T>,
    base_level: LevelFilter,
}

#[derive(Debug)]
pub struct ParseError {
    kind: ParseErrorKind,
}

#[derive(Debug)]
enum ParseErrorKind {
    Field(Box<dyn Error + Send + Sync>),
    Level(level::ParseError),
    Other,
}

impl Directive {
    pub(super) fn has_name(&self) -> bool {
        self.in_span.is_some()
    }

    pub(super) fn has_fields(&self) -> bool {
        !self.fields.is_empty()
    }

    pub(super) fn into_static(self) -> Result<StaticDirective, Self> {
        if self.is_dynamic() {
            return Err(self);
        }

        Ok(StaticDirective {
            target: self.target,
            level: self.level,
        })
    }

    pub(super) fn is_dynamic(&self) -> bool {
        self.has_name() || self.has_fields()
    }

    pub fn field_matcher(&self, meta: &Metadata) -> Option<field::CallsiteMatch> {
        let fieldset = meta.fields();
        let fields = self
            .fields
            .iter()
            .filter_map(
                |field::Match {
                     ref name,
                     ref value,
                 }| {
                    if let Some(field) = fieldset.field(name) {
                        let value = value.as_ref().cloned()?;
                        Some(Ok((field, value)))
                    } else {
                        Some(Err(()))
                    }
                },
            )
            .collect::<Result<FieldMap<_>, ()>>()
            .ok()?;
        Some(field::CallsiteMatch {
            fields,
            level: self.level.clone(),
        })
    }

    pub(super) fn make_tables(
        directives: impl IntoIterator<Item = Directive>,
    ) -> (Dynamics, Statics) {
        let (dyns, stats): (BTreeSet<Directive>, BTreeSet<Directive>) =
            directives.into_iter().partition(Directive::is_dynamic);
        let stats = stats.into_iter().filter_map(|d| d.into_static().ok());
        (Dynamics::from_iter(dyns), Statics::from_iter(stats))
    }
}

impl Match for Directive {
    fn cares_about(&self, meta: &Metadata) -> bool {
        // Does this directive have a target filter, and does it match the
        // metadata's target?
        if let Some(ref target) = self.target.as_ref() {
            if !meta.target().starts_with(&target[..]) {
                return false;
            }
        }

        // Do we have a name filter, and does it match the metadata's name?
        // TODO(eliza): put name globbing here?
        if let Some(ref name) = self.in_span {
            if name != meta.name() {
                return false;
            }
        }

        // Does the metadata define all the fields that this directive cares about?
        let fields = meta.fields();
        for field in &self.fields {
            if !fields.field(&field.name).is_some() {
                return false;
            }
        }

        true
    }

    fn level(&self) -> &LevelFilter {
        &self.level
    }
}

impl FromStr for Directive {
    type Err = ParseError;
    fn from_str(from: &str) -> Result<Self, Self::Err> {
        lazy_static! {
            static ref DIRECTIVE_RE: Regex = Regex::new(
                r"(?x)
                ^(?P<global_level>trace|TRACE|debug|DEBUG|info|INFO|warn|WARN|error|ERROR|off|OFF[0-5])$ |
                ^
                (?: # target name or span name
                    (?P<target>[\w:]+)|(?P<span>\[[^\]]*\])
                ){1,2}
                (?: # level or nothing
                    =(?P<level>trace|TRACE|debug|DEBUG|info|INFO|warn|WARN|error|ERROR|off|OFF[0-5])?
                )?
                $
                "
            )
            .unwrap();
            static ref SPAN_PART_RE: Regex =
                Regex::new(r#"(?P<name>\w+)?(?:\{(?P<fields>[^\}]*)\})?"#).unwrap();
            static ref FIELD_FILTER_RE: Regex =
                // TODO(eliza): this doesn't _currently_ handle value matchers that include comma
                // characters. We should fix that.
                Regex::new(r#"(?x)
                    (
                        # field name
                        [[:word:]][[[:word:]]\.]*
                        # value part (optional)
                        (?:=[^,]+)?
                    )
                    # trailing comma or EOS
                    (?:,\s?|$)
                "#).unwrap();
        }

        let caps = DIRECTIVE_RE.captures(from).ok_or_else(ParseError::new)?;

        if let Some(level) = caps
            .name("global_level")
            .and_then(|s| s.as_str().parse().ok())
        {
            return Ok(Directive {
                level,
                ..Default::default()
            });
        }

        let target = caps.name("target").and_then(|c| {
            let s = c.as_str();
            if s.parse::<LevelFilter>().is_ok() {
                None
            } else {
                Some(s.to_owned())
            }
        });

        let (in_span, fields) = caps
            .name("span")
            .and_then(|cap| {
                let cap = cap.as_str().trim_matches(|c| c == '[' || c == ']');
                let caps = SPAN_PART_RE.captures(cap)?;
                let span = caps.name("name").map(|c| c.as_str().to_owned());
                let fields = caps
                    .name("fields")
                    .map(|c| {
                        FIELD_FILTER_RE
                            .find_iter(c.as_str())
                            .map(|c| c.as_str().parse())
                            .collect::<Result<FilterVec<_>, _>>()
                    })
                    .unwrap_or_else(|| Ok(FilterVec::new()));
                Some((span, fields))
            })
            .unwrap_or_else(|| (None, Ok(FilterVec::new())));

        let level = caps
            .name("level")
            .and_then(|l| l.as_str().parse().ok())
            .unwrap_or(LevelFilter::ERROR);

        Ok(Directive {
            level,
            target,
            in_span,
            fields: fields?,
        })
    }
}

impl Default for Directive {
    fn default() -> Self {
        Directive {
            level: LevelFilter::OFF,
            target: None,
            in_span: None,
            fields: FilterVec::new(),
        }
    }
}

impl PartialOrd for Directive {
    fn partial_cmp(&self, other: &Directive) -> Option<Ordering> {
        match (self.has_name(), other.has_name()) {
            (true, false) => return Some(Ordering::Greater),
            (false, true) => return Some(Ordering::Less),
            _ => {}
        }

        match (self.fields.len(), other.fields.len()) {
            (a, b) if a == b => {}
            (a, b) => return Some(a.cmp(&b)),
        }

        match (self.target.as_ref(), other.target.as_ref()) {
            (Some(a), Some(b)) => Some(a.len().cmp(&b.len())),
            (Some(_), None) => Some(Ordering::Greater),
            (None, Some(_)) => Some(Ordering::Less),
            (None, None) => Some(Ordering::Equal),
        }
    }
}

impl Ord for Directive {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other)
            .expect("Directive::partial_cmp should define a total order")
    }
}

// === impl DirectiveSet ===

impl<T> DirectiveSet<T> {
    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }
}

impl<T: Ord> Default for DirectiveSet<T> {
    fn default() -> Self {
        Self {
            directives: BTreeSet::new(),
            max_level: LevelFilter::OFF,
        }
    }
}

impl<T: Match> DirectiveSet<T> {
    fn directives_for<'a>(
        &'a self,
        metadata: &'a Metadata<'a>,
    ) -> impl Iterator<Item = &'a T> + 'a {
        self.directives
            .iter()
            .rev()
            .filter(move |d| d.cares_about(metadata))
    }
}

impl<T: Match + Ord> FromIterator<T> for DirectiveSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut this = Self::default();
        this.extend(iter);
        this
    }
}

impl<T: Match + Ord> Extend<T> for DirectiveSet<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let max_level = &mut self.max_level;
        let ds = iter.into_iter().inspect(|d| {
            let level = d.level();
            if level > &*max_level {
                *max_level = level.clone();
            }
        });
        self.directives.extend(ds);
    }
}

// === impl Dynamics ===

impl Dynamics {
    pub fn matcher(&self, metadata: &Metadata) -> Option<CallsiteMatcher> {
        let mut base_level = None;
        let field_matches = self
            .directives_for(metadata)
            .filter_map(|d| {
                if let Some(f) = d.field_matcher(metadata) {
                    return Some(f);
                }
                match base_level {
                    Some(ref b) if &d.level > b => base_level = Some(d.level.clone()),
                    None => base_level = Some(d.level.clone()),
                    _ => {}
                }
                None
            })
            .collect();

        if let Some(base_level) = base_level {
            Some(CallsiteMatcher {
                field_matches,
                base_level,
            })
        } else if !field_matches.is_empty() {
            Some(CallsiteMatcher {
                field_matches,
                base_level: base_level.unwrap_or(LevelFilter::OFF),
            })
        } else {
            None
        }
    }
}


// === impl Statics ===

impl Statics {
    pub fn enabled(&self, meta: &Metadata) -> bool {
        let level = meta.level();
        self.directives_for(meta).any(|d| d.level >= *level)
    }

    pub fn add(&mut self, directive: StaticDirective) {
        if directive.level > self.max_level {
            self.max_level = directive.level.clone();
        }
        self.directives.insert(directive);
    }
}

impl PartialOrd for StaticDirective {
    fn partial_cmp(&self, other: &StaticDirective) -> Option<Ordering> {
        match (self.target.as_ref(), other.target.as_ref()) {
            (Some(a), Some(b)) => Some(a.len().cmp(&b.len())),
            (Some(_), None) => Some(Ordering::Greater),
            (None, Some(_)) => Some(Ordering::Less),
            (None, None) => Some(Ordering::Equal),
        }
    }
}

// ===== impl StaticDirective =====

impl Match for StaticDirective {
    fn cares_about(&self, meta: &Metadata) -> bool {
        // Does this directive have a target filter, and does it match the
        // metadata's target?
        if let Some(ref target) = self.target.as_ref() {
            if !meta.target().starts_with(&target[..]) {
                return false;
            }
        }

        true
    }

    fn level(&self) -> &LevelFilter {
        &self.level
    }
}

impl Default for StaticDirective {
    fn default() -> Self {
        StaticDirective {
            target: None,
            level: LevelFilter::ERROR,
        }
    }
}

// ===== impl ParseError =====

impl ParseError {
    fn new() -> Self {
        ParseError {
            kind: ParseErrorKind::Other,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.kind {
            ParseErrorKind::Other => f.pad("invalid filter directive"),
            ParseErrorKind::Level(ref l) => l.fmt(f),
            ParseErrorKind::Field(ref e) => write!(f, "invalid field filter: {}", e),
        }
    }
}

impl Error for ParseError {
    fn description(&self) -> &str {
        "invalid filter directive"
    }

    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.kind {
            ParseErrorKind::Other => None,
            ParseErrorKind::Level(ref l) => Some(l),
            ParseErrorKind::Field(ref n) => Some(n.as_ref()),
        }
    }
}

impl From<Box<dyn Error + Send + Sync>> for ParseError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        Self {
            kind: ParseErrorKind::Field(e),
        }
    }
}

impl From<level::ParseError> for ParseError {
    fn from(l: level::ParseError) -> Self {
        Self {
            kind: ParseErrorKind::Level(l),
        }
    }
}

// ===== impl DynamicMatch =====

impl CallsiteMatcher {
    /// Create a new `SpanMatch` for a given instance of the matched callsite.
    pub fn to_span_match(&self, attrs: &span::Attributes) -> SpanMatcher {
        let field_matches = self
            .field_matches
            .iter()
            .map(|m| {
                let m = m.to_span_match();
                attrs.record(&mut m.visitor());
                m
            })
            .collect();
        SpanMatcher {
            field_matches,
            base_level: self.base_level.clone(),
        }
    }
}

impl SpanMatcher {
    /// Returns the level currently enabled for this callsite.
    pub fn level(&self) -> LevelFilter {
        self.field_matches
            .iter()
            .filter_map(|f| {
                if f.is_matched() {
                    Some(f.level())
                } else {
                    None
                }
            })
            .max()
            .unwrap_or_else(|| self.base_level.clone())
    }

    pub fn record_update(&self, record: &span::Record) {
        for m in &self.field_matches {
            record.record(&mut m.visitor())
        }
    }
}
