use bon::Builder;
use string_compare_builder::{
    IsUnset, SetIgnoreLeadingTheInA, SetIgnoreLeadingTheInB, State,
};
use unicase::UniCase;
#[derive(Debug, Default, Builder)]
pub struct StringCompare {
    #[builder(default)]
    fold_case: bool,
    #[builder(default)]
    ignore_leading_the_in_a: bool,
    #[builder(default)]
    ignore_leading_the_in_b: bool,
}
impl<S: State> StringCompareBuilder<S> {
    pub fn ignore_leading_the(
        self,
        value: bool,
    ) -> StringCompareBuilder<SetIgnoreLeadingTheInB<SetIgnoreLeadingTheInA<S>>>
    where
        S::IgnoreLeadingTheInA: IsUnset,
        S::IgnoreLeadingTheInB: IsUnset,
    {
        self.ignore_leading_the_in_a(value).ignore_leading_the_in_b(value)
    }
}
fn strip_the(input: &str) -> &str {
    if input.len() < 4 {
        return input;
    }
    let Some(s) = input.get(..4) else {
        return input;
    };
    if s == "THE " || s == "the " || s == "The " {
        return &input[4..];
    }
    return input;
}
#[allow(unused)]
impl StringCompare {
    pub fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        let a = if self.ignore_leading_the_in_a { strip_the(a) } else { a };
        let b = if self.ignore_leading_the_in_b { strip_the(b) } else { b };
        if self.fold_case { UniCase::new(a).cmp(&UniCase::new(b)) } else { a.cmp(b) }
    }
    pub fn eq(&self, a: &str, b: &str) -> bool {
        let a = if self.ignore_leading_the_in_a { strip_the(a) } else { a };
        let b = if self.ignore_leading_the_in_b { strip_the(b) } else { b };
        if self.fold_case { UniCase::new(a) == UniCase::new(b) } else { a == b }
    }
}
