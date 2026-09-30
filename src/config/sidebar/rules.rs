use serde::{Deserialize, Serialize};

use super::{SidebarTokenColor, SidebarTokenStyle};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawRule", into = "RawRule")]
pub struct SidebarTokenRule {
    condition: Condition,
    // source names another custom token (without the `$`) whose value the condition reads instead
    // of the styled token's own value, e.g. colour `machine` by `$ctx` (herdr-upm).
    source: Option<String>,
    ignore_case: bool,
    style: SidebarTokenStyle,
    hide: Option<bool>,
}

// Deserialization rejects non-finite thresholds, so equality is reflexive.
impl Eq for SidebarTokenRule {}

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    Equals(String),
    Contains(String),
    StartsWith(String),
    GreaterThan(f64),
    LessThan(f64),
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    equals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contains: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    starts_with: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gt: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lt: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_case: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fg: Option<SidebarTokenColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dim: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hide: Option<bool>,
}

impl TryFrom<RawRule> for SidebarTokenRule {
    type Error = String;

    fn try_from(raw: RawRule) -> Result<Self, Self::Error> {
        let count = [
            raw.equals.is_some(),
            raw.contains.is_some(),
            raw.starts_with.is_some(),
            raw.gt.is_some(),
            raw.lt.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if count != 1 {
            return Err(
                "sidebar rule requires exactly one of equals, contains, starts_with, gt, lt".into(),
            );
        }
        let condition = if let Some(value) = raw.equals {
            Condition::Equals(value)
        } else if let Some(value) = raw.contains {
            Condition::Contains(value)
        } else if let Some(value) = raw.starts_with {
            Condition::StartsWith(value)
        } else {
            let (value, greater) = match (raw.gt, raw.lt) {
                (Some(value), _) => (value, true),
                (_, Some(value)) => (value, false),
                _ => unreachable!("validated condition count"),
            };
            if !value.is_finite() {
                return Err("sidebar numeric rule threshold must be finite".into());
            }
            if raw.ignore_case.is_some() {
                return Err("ignore_case applies only to sidebar text conditions".into());
            }
            if greater {
                Condition::GreaterThan(value)
            } else {
                Condition::LessThan(value)
            }
        };
        let source = match raw.source {
            None => None,
            Some(value) => Some(parse_source(&value)?),
        };
        Ok(Self {
            condition,
            source,
            ignore_case: raw.ignore_case.unwrap_or(false),
            hide: raw.hide,
            style: SidebarTokenStyle {
                fg: raw.fg,
                bold: raw.bold,
                dim: raw.dim,
            },
        })
    }
}

impl From<SidebarTokenRule> for RawRule {
    fn from(rule: SidebarTokenRule) -> Self {
        let mut raw = Self {
            source: rule.source.map(|name| format!("${name}")),
            ignore_case: rule.ignore_case.then_some(true),
            fg: rule.style.fg,
            bold: rule.style.bold,
            dim: rule.style.dim,
            hide: rule.hide,
            ..Self::default()
        };
        match rule.condition {
            Condition::Equals(value) => raw.equals = Some(value),
            Condition::Contains(value) => raw.contains = Some(value),
            Condition::StartsWith(value) => raw.starts_with = Some(value),
            Condition::GreaterThan(value) => raw.gt = Some(value),
            Condition::LessThan(value) => raw.lt = Some(value),
        }
        raw
    }
}

impl SidebarTokenRule {
    fn matches(&self, value: &str, numeric: &mut Option<Option<f64>>) -> bool {
        match &self.condition {
            Condition::Equals(expected) => {
                if self.ignore_case {
                    value.eq_ignore_ascii_case(expected)
                } else {
                    value == expected
                }
            }
            Condition::StartsWith(expected) => {
                if self.ignore_case {
                    value
                        .as_bytes()
                        .get(..expected.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(expected.as_bytes()))
                } else {
                    value.starts_with(expected)
                }
            }
            Condition::Contains(expected) => {
                if self.ignore_case {
                    expected.is_empty()
                        || value
                            .as_bytes()
                            .windows(expected.len())
                            .any(|part| part.eq_ignore_ascii_case(expected.as_bytes()))
                } else {
                    value.contains(expected)
                }
            }
            Condition::GreaterThan(threshold) | Condition::LessThan(threshold) => {
                let parsed = numeric.get_or_insert_with(|| parse_number(value));
                parsed.is_some_and(|number| match self.condition {
                    Condition::GreaterThan(_) => number > *threshold,
                    _ => number < *threshold,
                })
            }
        }
    }
}

// parse_number reads a full, finite number and nothing else: no unit, no padding (the upstream
// contract numeric_conditions_require_full_finite_numbers_and_strict_comparison pins). A
// percentage source therefore needs a numeric token, e.g. herdr-ccwait's `$ctx_num` (herdr-upm).
fn parse_number(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

// parse_source validates a rule `source`: a custom token reference, `$` plus 1-32 of
// [A-Za-z0-9_-], the same shape parse_sidebar_token accepts. Stored without the `$`.
fn parse_source(value: &str) -> Result<String, String> {
    let name = value
        .strip_prefix('$')
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 32
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        })
        .ok_or_else(|| {
            format!("sidebar rule source `{value}` must be a custom token like `$ctx`")
        })?;
    Ok(name.to_string())
}

// matching_style returns the style of the first rule that matches. A rule with a `source` reads
// that custom token through `lookup`; when the token is absent the rule does not match.
pub(super) fn matching_style<'v>(
    rules: &[SidebarTokenRule],
    base: SidebarTokenStyle,
    value: &str,
    lookup: &dyn Fn(&str) -> Option<&'v str>,
) -> Option<SidebarTokenStyle> {
    let mut numeric = None;
    for rule in rules {
        let matched = match &rule.source {
            None => rule.matches(value, &mut numeric),
            Some(name) => lookup(name).is_some_and(|other| rule.matches(other, &mut None)),
        };
        if matched {
            if rule.hide == Some(true) {
                return None;
            }
            return Some(SidebarTokenStyle {
                fg: rule.style.fg.or(base.fg),
                bold: rule.style.bold.or(base.bold),
                dim: rule.style.dim.or(base.dim),
            });
        }
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_conditions_use_exact_case_or_ascii_folding() {
        for (condition, yes, no) in [
            ("equals = 'Local'", "Local", "Localhost"),
            ("contains = 'Local'", "myLocalbox", "remote"),
            ("starts_with = 'Local'", "Localhost", "myLocal"),
        ] {
            let rule: SidebarTokenRule = toml::from_str(condition).unwrap();
            assert!(rule.matches(yes, &mut None));
            assert!(!rule.matches(no, &mut None));
            assert!(!rule.matches(&yes.to_ascii_lowercase(), &mut None));
            let folded: SidebarTokenRule =
                toml::from_str(&format!("{condition}\nignore_case = true")).unwrap();
            assert!(folded.matches(&yes.to_ascii_lowercase(), &mut None));
        }
        for condition in ["equals", "contains", "starts_with"] {
            let rule: SidebarTokenRule =
                toml::from_str(&format!("{condition} = 'ÉA'\nignore_case = true")).unwrap();
            assert!(rule.matches("Éa", &mut None));
            assert!(!rule.matches("éa", &mut None));
        }
        let empty: SidebarTokenRule = toml::from_str("contains = ''\nignore_case = true").unwrap();
        assert!(empty.matches("", &mut None));
    }

    #[test]
    fn numeric_conditions_require_full_finite_numbers_and_strict_comparison() {
        for (condition, yes, no) in [
            ("gt = 80", ["90", "8.1e1", "+90"], "70"),
            ("lt = 80", ["70", "7.9e1", "-90"], "90"),
        ] {
            let rule: SidebarTokenRule = toml::from_str(condition).unwrap();
            for value in yes {
                assert!(rule.matches(value, &mut None), "{condition}: {value}");
            }
            for value in [
                no, "80", "90%", " 90", "90 ", "", "NaN", "inf", "-inf", "1e999",
            ] {
                assert!(!rule.matches(value, &mut None), "{condition}: {value}");
            }
        }
    }

    // herdr-upm: a rule with `source` reads another custom token of the pane.
    #[test]
    fn source_rules_read_another_token_and_accept_percent() {
        let rules: Vec<SidebarTokenRule> = [
            "source = '$ctx_num'\ngt = 80\nbold = true",
            "source = '$ctx_num'\ngt = 50\ndim = true",
        ]
        .iter()
        .map(|raw| toml::from_str(raw).unwrap())
        .collect();
        let base = SidebarTokenStyle::default();
        let style = |ctx: Option<&str>| {
            matching_style(&rules, base, "dcc", &|name| {
                assert_eq!(name, "ctx_num");
                ctx
            })
        };
        assert_eq!(style(Some("85")).unwrap().bold, Some(true));
        assert_eq!(style(Some("60")).unwrap().dim, Some(true));
        assert_eq!(style(Some("60")).unwrap().bold, None);
        assert_eq!(style(Some("10")).unwrap(), base);
        // the upstream numeric contract holds for a sourced value too: a unit is not a number
        assert_eq!(style(Some("85%")).unwrap(), base);
        // an absent source token matches no rule, whatever the styled value is
        assert_eq!(style(None).unwrap(), base);
        // the styled value itself is never read by a sourced rule
        assert_eq!(
            matching_style(&rules, base, "99", &|_| Some("1")).unwrap(),
            base
        );
    }

    #[test]
    fn source_must_be_a_custom_token_and_round_trips() {
        for bad in ["ctx", "$", "$bad name", "$ctx.x"] {
            let raw = format!("source = '{bad}'\ngt = 1");
            assert!(toml::from_str::<SidebarTokenRule>(&raw).is_err(), "{bad}");
        }
        let rule: SidebarTokenRule = toml::from_str("source = '$ctx_num'\ngt = 80").unwrap();
        let back = toml::to_string(&rule).unwrap();
        assert!(back.contains("source = \"$ctx_num\""), "{back}");
        assert_eq!(toml::from_str::<SidebarTokenRule>(&back).unwrap(), rule);
    }
}
