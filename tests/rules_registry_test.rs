use lint4d::engine::Severity;
use lint4d::rules::{RuleCategory, RuleRegistry};

#[test]
fn registry_returns_all_rules() {
    let registry = RuleRegistry::new();
    let rules = registry.all_rules();
    assert_eq!(rules.len(), 13);
}

#[test]
fn registry_get_by_id() {
    let registry = RuleRegistry::new();
    let rule = registry.get("empty-except");
    assert!(rule.is_some());
    let meta = rule.unwrap().meta();
    assert_eq!(meta.id, "empty-except");
    assert_eq!(meta.default_severity, Severity::Warning);
}

#[test]
fn registry_get_nonexistent_returns_none() {
    let registry = RuleRegistry::new();
    assert!(registry.get("nonexistent-rule").is_none());
}

#[test]
fn all_rule_ids_are_unique() {
    let registry = RuleRegistry::new();
    let rules = registry.all_rules();
    let mut ids: Vec<&str> = rules.iter().map(|r| r.meta().id).collect();
    let len_before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "Duplicate rule IDs found");
}

#[test]
fn rule_categories_exist() {
    let registry = RuleRegistry::new();
    let rules = registry.all_rules();
    let categories: Vec<RuleCategory> = rules.iter().map(|r| r.meta().category).collect();
    assert!(categories.contains(&RuleCategory::ResourceManagement));
    assert!(categories.contains(&RuleCategory::ExceptionHandling));
    assert!(categories.contains(&RuleCategory::NamingConvention));
    assert!(categories.contains(&RuleCategory::DangerousPattern));
}
