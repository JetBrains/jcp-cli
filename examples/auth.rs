use jcp::{
    auth::{get_access_tokens, login},
    keychain,
};

fn main() {
    let keychain = keychain::active_keychain();
    let refresh_token = keychain
        .get_refresh_token()
        .expect("Unable to read keychain")
        .unwrap_or_else(|| login().expect("Unable to login"));

    let token = get_access_tokens(&refresh_token).unwrap();

    eprintln!("=== Authentication Successful ===\n");
    eprintln!("JCP Access token:\n  {}\n", token.jcp_access_token);
    eprintln!("JB AI Access token:\n  {}\n", token.ai_access_token);
}
