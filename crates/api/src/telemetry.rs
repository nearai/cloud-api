use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

pub(crate) fn build_telemetry_resource(environment: &str, instance_id: Option<&str>) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", "cloud-api"),
        KeyValue::new("environment", environment.to_string()),
    ];
    if let Some(instance_id) = instance_id {
        attributes.push(KeyValue::new(
            "service.instance.id",
            instance_id.to_string(),
        ));
    }

    Resource::builder().with_attributes(attributes).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Key;

    #[test]
    fn telemetry_resource_sets_configured_instance_id() {
        let resource = build_telemetry_resource("staging", Some("vmm-instance-123"));

        assert_eq!(
            resource
                .get(&Key::new("service.instance.id"))
                .map(|value| value.to_string()),
            Some("vmm-instance-123".to_string())
        );
    }

    #[test]
    fn telemetry_resource_omits_unset_instance_id() {
        let resource = build_telemetry_resource("local", None);

        assert_eq!(
            resource
                .get(&Key::new("service.name"))
                .map(|value| value.to_string()),
            Some("cloud-api".to_string())
        );
        assert_eq!(
            resource
                .get(&Key::new("environment"))
                .map(|value| value.to_string()),
            Some("local".to_string())
        );
        assert!(resource.get(&Key::new("service.instance.id")).is_none());
    }
}
