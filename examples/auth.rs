use jcp::{
    EnvConfig,
    auth::{get_access_token, login},
    keychain,
};

fn main() {
    let keychain = keychain::active_keychain();
    let staging = EnvConfig::staging();
    let refresh_token = keychain
        .get_refresh_token()
        .expect("Unable to read keychain")
        .unwrap_or_else(|| login(&staging).expect("Unable to login"));

    let token = get_access_token(&refresh_token, &staging).unwrap();

    eprintln!("=== Authentication Successful ===\n");
    eprintln!("JCP Access token:\n  {}\n", token);
}
