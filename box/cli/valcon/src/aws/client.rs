use aws_config::{BehaviorVersion, Region, SdkConfig};

/// Shared SDK config. Unset profile and region fall back to the SDK chain (`AWS_PROFILE`, `AWS_REGION`, profile file).
pub async fn sdk_config(profile: Option<&str>, region: Option<&str>) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }
    if let Some(r) = region {
        loader = loader.region(Region::new(r.to_owned()));
    }
    loader.load().await
}
