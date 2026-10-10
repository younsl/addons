//! Mapping between Application names and the promotion chain.

/// Derives the environment-independent application identity from an Argo CD
/// Application name.
///
/// The convention is `<env>-<identity>`, where env is the Application's
/// `spec.project`. A name without that prefix is returned unchanged, so an app
/// named after its own project still gets a usable identity.
#[must_use]
pub fn identity_of(name: &str, project: &str) -> String {
    let prefix = format!("{project}-");
    name.strip_prefix(prefix.as_str())
        .unwrap_or(name)
        .to_string()
}

/// Composes the Application name for an identity in an environment.
#[must_use]
pub fn app_name_for(env: &str, identity: &str) -> String {
    format!("{env}-{identity}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_strips_the_project_prefix() {
        assert_eq!(identity_of("prd-payment-api", "prd"), "payment-api");
        assert_eq!(identity_of("payment-api", "prd"), "payment-api");
        assert_eq!(identity_of("prd", "prd"), "prd");
        assert_eq!(identity_of("prd-prd-api", "prd"), "prd-api");
    }

    #[test]
    fn app_name_round_trips() {
        let name = app_name_for("stg", "payment-api");
        assert_eq!(name, "stg-payment-api");
        assert_eq!(identity_of(&name, "stg"), "payment-api");
    }
}
