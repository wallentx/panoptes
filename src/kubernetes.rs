//! Repository-declared Kubernetes resources. No cluster or credentials are read.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Dialect, Link, Target, Yaml};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub symbol: usize,
    pub key: Key,
    pub version: String,
    pub labels: Vec<(String, String)>,
    pub pod_labels: Option<Vec<(String, String)>>,
}
fn at<'a>(mut node: Node<'a>, yaml: &Yaml<'a, '_>, keys: &[&str]) -> Option<Node<'a>> {
    for key in keys {
        node = yaml::get(node, yaml, key)?;
    }
    Some(node)
}
fn text<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>, keys: &[&str]) -> Option<String> {
    yaml::literal(at(node, yaml, keys)?, yaml)
}
fn labels<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>) -> Option<Vec<(String, String)>> {
    yaml::pairs(node, yaml)
        .into_iter()
        .map(|(k, v)| Some((k, yaml::literal(v, yaml)?)))
        .collect()
}
/// Built-in API scopes, keyed by group as well as kind to avoid collisions
/// with custom resources. Source: Kubernetes api/openapi-spec/swagger.json.
fn cluster_scoped(group: &str, kind: &str) -> bool {
    matches!(
        (group, kind),
        ("", "Namespace" | "Node" | "PersistentVolume")
            | (
                "storage.k8s.io",
                "StorageClass"
                    | "CSIDriver"
                    | "CSINode"
                    | "VolumeAttachment"
                    | "VolumeAttributesClass"
            )
            | (
                "networking.k8s.io",
                "IngressClass" | "IPAddress" | "ServiceCIDR"
            )
            | (
                "certificates.k8s.io",
                "CertificateSigningRequest" | "ClusterTrustBundle"
            )
            | (
                "flowcontrol.apiserver.k8s.io",
                "FlowSchema" | "PriorityLevelConfiguration"
            )
            | ("internal.apiserver.k8s.io", "StorageVersion")
            | ("storagemigration.k8s.io", "StorageVersionMigration")
            | (
                "resource.k8s.io",
                "DeviceClass" | "ResourceSlice" | "DeviceTaintRule" | "ResourcePoolStatusRequest"
            )
            | ("authentication.k8s.io", "TokenReview" | "SelfSubjectReview")
            | (
                "authorization.k8s.io",
                "SubjectAccessReview" | "SelfSubjectAccessReview" | "SelfSubjectRulesReview"
            )
            | (
                "rbac.authorization.k8s.io",
                "ClusterRole" | "ClusterRoleBinding"
            )
            | ("apiextensions.k8s.io", "CustomResourceDefinition")
            | ("apiregistration.k8s.io", "APIService")
            | ("node.k8s.io", "RuntimeClass")
            | ("scheduling.k8s.io", "PriorityClass")
            | (
                "admissionregistration.k8s.io",
                "MutatingWebhookConfiguration"
                    | "ValidatingWebhookConfiguration"
                    | "ValidatingAdmissionPolicy"
                    | "ValidatingAdmissionPolicyBinding"
                    | "MutatingAdmissionPolicy"
                    | "MutatingAdmissionPolicyBinding"
            )
    )
}

fn reference(
    ex: &mut Extracted,
    from: usize,
    group: &str,
    kind: &str,
    namespace: &str,
    name: String,
) {
    ex.automation.links.push(Link {
        from: Some(from),
        scope: None,
        target: Target::Kube(Key {
            group: group.to_string(),
            kind: kind.to_string(),
            namespace: if cluster_scoped(group, kind) {
                String::new()
            } else {
                namespace.to_string()
            },
            name,
        }),
    });
}
fn named<'a>(
    ex: &mut Extracted,
    from: usize,
    node: Node<'a>,
    yaml: &Yaml<'a, '_>,
    namespace: &str,
    kind: &str,
    keys: &[&str],
) {
    if let Some(name) = text(node, yaml, keys) {
        reference(ex, from, "", kind, namespace, name);
    }
}

pub fn enrich<'a>(root: Node<'a>, yaml: &Yaml<'a, '_>, ex: &mut Extracted) {
    if ex.automation.dialect != Dialect::Generic {
        return;
    }
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        resource(doc, yaml, ex);
    }
}
fn resource<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>, ex: &mut Extracted) {
    let Some(api) = text(node, yaml, &["apiVersion"]) else {
        return;
    };
    let Some(kind) = text(node, yaml, &["kind"]) else {
        return;
    };
    if kind == "List" {
        if let Some(items) = yaml::get(node, yaml, "items") {
            for item in yaml::items(items, yaml) {
                resource(item, yaml, ex);
            }
        }
        return;
    }
    if api.starts_with("kustomize.config.k8s.io/") {
        return;
    }
    let Some(name) = text(node, yaml, &["metadata", "name"]) else {
        return;
    };
    let (group, version) = api.split_once('/').unwrap_or(("", &api));
    let namespace = if cluster_scoped(group, &kind) {
        String::new()
    } else {
        match at(node, yaml, &["metadata", "namespace"]) {
            Some(value) => {
                let Some(namespace) = yaml::literal(value, yaml) else {
                    return;
                };
                namespace
            }
            None => "default".to_string(),
        }
    };
    let display = if namespace.is_empty() {
        name.clone()
    } else {
        format!("{namespace}/{name}")
    };
    let id = yaml::symbol(
        ex,
        yaml,
        node,
        format!("{kind}: {display}"),
        "resource",
        None,
    );
    ex.automation.dialect = Dialect::Kubernetes;
    ex.automation.links.push(Link {
        from: None,
        scope: None,
        target: Target::Symbol(id),
    });
    let builtin = matches!(
        group,
        "" | "apps"
            | "batch"
            | "networking.k8s.io"
            | "rbac.authorization.k8s.io"
            | "autoscaling"
            | "storage.k8s.io"
    );
    let template = match kind.as_str() {
        "Pod" => Some(node),
        "Deployment"
        | "ReplicaSet"
        | "StatefulSet"
        | "DaemonSet"
        | "Job"
        | "ReplicationController" => at(node, yaml, &["spec", "template"]),
        "CronJob" => at(node, yaml, &["spec", "jobTemplate", "spec", "template"]),
        _ => None,
    };
    let template = template.filter(|_| builtin);
    let pod_labels = template.and_then(|n| match at(n, yaml, &["metadata", "labels"]) {
        Some(n) => labels(n, yaml),
        None => Some(Vec::new()),
    });
    ex.automation.resources.push(Resource {
        symbol: id,
        key: Key {
            group: group.to_string(),
            kind: kind.clone(),
            namespace: namespace.clone(),
            name,
        },
        version: version.to_string(),
        labels: at(node, yaml, &["metadata", "labels"])
            .and_then(|n| labels(n, yaml))
            .unwrap_or_default(),
        pod_labels,
    });
    if !builtin {
        return;
    }
    if let Some(spec) = template.and_then(|n| yaml::get(n, yaml, "spec")) {
        pod(ex, id, spec, yaml, &namespace);
    }
    match kind.as_str() {
        "Service" => {
            if let Some(selector) =
                at(node, yaml, &["spec", "selector"]).and_then(|n| labels(n, yaml))
                && !selector.is_empty()
            {
                ex.automation.links.push(Link {
                    from: Some(id),
                    scope: None,
                    target: Target::SelectPods {
                        namespace: namespace.clone(),
                        labels: selector,
                    },
                });
            }
        }
        "StatefulSet" => named(
            ex,
            id,
            node,
            yaml,
            &namespace,
            "Service",
            &["spec", "serviceName"],
        ),
        "Ingress" => {
            if let Some(spec) = yaml::get(node, yaml, "spec") {
                if let Some(backend) = yaml::get(spec, yaml, "defaultBackend") {
                    ingress_backend(ex, id, backend, yaml, &namespace);
                }
                if let Some(rules) = yaml::get(spec, yaml, "rules") {
                    for rule in yaml::items(rules, yaml) {
                        if let Some(paths) = at(rule, yaml, &["http", "paths"]) {
                            for path in yaml::items(paths, yaml) {
                                if let Some(backend) = yaml::get(path, yaml, "backend") {
                                    ingress_backend(ex, id, backend, yaml, &namespace);
                                }
                            }
                        }
                    }
                }
                if let Some(tls) = yaml::get(spec, yaml, "tls") {
                    for item in yaml::items(tls, yaml) {
                        named(ex, id, item, yaml, &namespace, "Secret", &["secretName"]);
                    }
                }
            }
        }
        "RoleBinding" | "ClusterRoleBinding" => {
            if let Some(role) = yaml::get(node, yaml, "roleRef")
                && let (Some(kind), Some(name), Some(group)) = (
                    text(role, yaml, &["kind"]),
                    text(role, yaml, &["name"]),
                    text(role, yaml, &["apiGroup"]),
                )
            {
                reference(ex, id, &group, &kind, &namespace, name);
            }
            if let Some(subjects) = yaml::get(node, yaml, "subjects") {
                for subject in yaml::items(subjects, yaml) {
                    if text(subject, yaml, &["kind"]).as_deref() == Some("ServiceAccount") {
                        let ns = text(subject, yaml, &["namespace"])
                            .or_else(|| (kind == "RoleBinding").then(|| namespace.clone()));
                        if let Some(ns) = ns {
                            named(ex, id, subject, yaml, &ns, "ServiceAccount", &["name"]);
                        }
                    }
                }
            }
        }
        "HorizontalPodAutoscaler" => {
            if let Some(target) = at(node, yaml, &["spec", "scaleTargetRef"])
                && let (Some(api), Some(kind), Some(name)) = (
                    text(target, yaml, &["apiVersion"]),
                    text(target, yaml, &["kind"]),
                    text(target, yaml, &["name"]),
                )
            {
                reference(
                    ex,
                    id,
                    api.split_once('/').map_or("", |(g, _)| g),
                    &kind,
                    &namespace,
                    name,
                );
            }
        }
        "PersistentVolumeClaim" => {
            named(
                ex,
                id,
                node,
                yaml,
                "",
                "PersistentVolume",
                &["spec", "volumeName"],
            );
            if let Some(name) = text(node, yaml, &["spec", "storageClassName"]) {
                reference(ex, id, "storage.k8s.io", "StorageClass", "", name);
            }
        }
        _ => {}
    }
}
fn ingress_backend<'a>(
    ex: &mut Extracted,
    id: usize,
    node: Node<'a>,
    yaml: &Yaml<'a, '_>,
    namespace: &str,
) {
    if let Some(name) =
        text(node, yaml, &["service", "name"]).or_else(|| text(node, yaml, &["serviceName"]))
    {
        reference(ex, id, "", "Service", namespace, name);
    }
}
fn pod<'a>(ex: &mut Extracted, id: usize, spec: Node<'a>, yaml: &Yaml<'a, '_>, namespace: &str) {
    named(
        ex,
        id,
        spec,
        yaml,
        namespace,
        "ServiceAccount",
        &["serviceAccountName"],
    );
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(containers) = yaml::get(spec, yaml, field) {
            for container in yaml::items(containers, yaml) {
                if let Some(env) = yaml::get(container, yaml, "env") {
                    for entry in yaml::items(env, yaml) {
                        named(
                            ex,
                            id,
                            entry,
                            yaml,
                            namespace,
                            "ConfigMap",
                            &["valueFrom", "configMapKeyRef", "name"],
                        );
                        named(
                            ex,
                            id,
                            entry,
                            yaml,
                            namespace,
                            "Secret",
                            &["valueFrom", "secretKeyRef", "name"],
                        );
                    }
                }
                if let Some(env) = yaml::get(container, yaml, "envFrom") {
                    for entry in yaml::items(env, yaml) {
                        named(
                            ex,
                            id,
                            entry,
                            yaml,
                            namespace,
                            "ConfigMap",
                            &["configMapRef", "name"],
                        );
                        named(
                            ex,
                            id,
                            entry,
                            yaml,
                            namespace,
                            "Secret",
                            &["secretRef", "name"],
                        );
                    }
                }
            }
        }
    }
    if let Some(secrets) = yaml::get(spec, yaml, "imagePullSecrets") {
        for secret in yaml::items(secrets, yaml) {
            named(ex, id, secret, yaml, namespace, "Secret", &["name"]);
        }
    }
    if let Some(volumes) = yaml::get(spec, yaml, "volumes") {
        for volume in yaml::items(volumes, yaml) {
            named(
                ex,
                id,
                volume,
                yaml,
                namespace,
                "ConfigMap",
                &["configMap", "name"],
            );
            named(
                ex,
                id,
                volume,
                yaml,
                namespace,
                "Secret",
                &["secret", "secretName"],
            );
            named(
                ex,
                id,
                volume,
                yaml,
                namespace,
                "PersistentVolumeClaim",
                &["persistentVolumeClaim", "claimName"],
            );
            if let Some(sources) = at(volume, yaml, &["projected", "sources"]) {
                for source in yaml::items(sources, yaml) {
                    named(
                        ex,
                        id,
                        source,
                        yaml,
                        namespace,
                        "ConfigMap",
                        &["configMap", "name"],
                    );
                    named(
                        ex,
                        id,
                        source,
                        yaml,
                        namespace,
                        "Secret",
                        &["secret", "name"],
                    );
                }
            }
        }
    }
}

pub fn resolve(pending: &[Pending]) -> Resolved {
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    let mut resources: HashMap<&Key, Vec<i64>> = HashMap::new();
    let patches = crate::kustomize::patch_only_paths(pending);
    for file in pending.iter().filter(|f| !patches.contains(&f.rel)) {
        for resource in &file.extracted.automation.resources {
            resources
                .entry(&resource.key)
                .or_default()
                .push(file.symbol_ids[resource.symbol]);
        }
    }
    for file in pending.iter().filter(|f| {
        f.extracted.automation.dialect == Dialect::Kubernetes && !patches.contains(&f.rel)
    }) {
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            let targets = match &link.target {
                Target::Symbol(i) => vec![file.symbol_ids[*i]],
                Target::Kube(key) => resources
                    .get(key)
                    .filter(|ids| ids.len() == 1)
                    .cloned()
                    .unwrap_or_default(),
                Target::SelectPods { namespace, labels } => pending
                    .iter()
                    .filter(|f| !patches.contains(&f.rel))
                    .flat_map(|f| {
                        f.extracted.automation.resources.iter().filter_map(|r| {
                            (r.key.namespace == *namespace
                                && r.pod_labels
                                    .as_ref()
                                    .is_some_and(|have| labels.iter().all(|p| have.contains(p)))
                                && resources.get(&r.key).is_some_and(|ids| ids.len() == 1))
                            .then_some(f.symbol_ids[r.symbol])
                        })
                    })
                    .collect(),
                _ => continue,
            };
            if targets.is_empty() {
                result.unresolved += 1;
            }
            for target in targets {
                result.edges.push((from, target, "calls"));
            }
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}
