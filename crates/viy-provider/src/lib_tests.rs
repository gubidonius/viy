//! The seam's own parts: an error reads as its message, and a provider
//! without volumes says so (kb d182).

use pretty_assertions::assert_eq;

use super::*;

struct Bare;

impl Provider for Bare {
    fn name(&self) -> &'static str {
        "bare"
    }
    fn pods(&self) -> BoxFuture<'_, Result<Vec<Pod>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn pod<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Option<Pod>>> {
        Box::pin(async { Ok(None) })
    }
    fn create<'a>(&'a self, request: &'a PodRequest) -> BoxFuture<'a, Result<Pod>> {
        Box::pin(async move {
            Ok(Pod {
                id: "p1".into(),
                name: Some(request.name.clone()),
                state: Some(PodState::Starting),
                ..Pod::default()
            })
        })
    }
    fn start<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn terminate<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn logs<'a>(&'a self, _: &'a str, _: u32, _: Duration) -> BoxFuture<'a, Result<Vec<LogLine>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[test]
fn a_provider_without_volumes_has_none() {
    let bare: &dyn Provider = &Bare;
    assert_eq!(bare.name(), "bare");
    assert!(bare.volumes().is_none());
}

#[test]
fn an_error_reads_as_its_message() {
    let error = ProviderError {
        message: "POST /pods -> HTTP 400\nno instances".into(),
        status: Some(400),
        capacity: true,
        transient: false,
        retry_after: None,
    };
    assert_eq!(error.to_string(), "POST /pods -> HTTP 400\nno instances");
}

#[test]
fn a_requests_debug_leaves_out_secret_values() {
    let request = PodRequest {
        name: "pod".into(),
        env: vec![("HF_TOKEN".into(), "hf_SYNTHETICSECRET".into())],
        command: vec!["export KEY=sk-SYNTHETIC".into()],
        ..PodRequest::default()
    };
    let shown = format!("{request:?}");
    assert!(shown.contains("HF_TOKEN"), "{shown}");
    assert!(
        !shown.contains("hf_SYNTHETICSECRET") && !shown.contains("sk-SYNTHETIC"),
        "{shown}"
    );
}
