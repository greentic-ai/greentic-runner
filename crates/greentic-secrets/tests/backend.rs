use greentic_secrets::{SecretsBackend, init};

#[test]
fn parses_backends() {
    assert_eq!(
        "env".parse::<SecretsBackend>().unwrap(),
        SecretsBackend::Env
    );
    assert_eq!(
        "AWS".parse::<SecretsBackend>().unwrap(),
        SecretsBackend::Aws
    );
    assert!("unknown".parse::<SecretsBackend>().is_err());
}

#[test]
fn init_env_backend() {
    init(SecretsBackend::Env).expect("env backend should initialise");
}
