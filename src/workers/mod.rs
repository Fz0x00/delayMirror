pub mod allowlist;
pub mod handlers;
pub mod router;

pub use allowlist::{AllowlistConfig, AllowlistManager};
pub use router::route_request;

#[cfg(feature = "workers")]
use worker::*;

#[cfg(feature = "workers")]
#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    use crate::core::{Config, DelayChecker};

    let config = Config::from_env_vars(|key| {
        env.var(key).ok().map(|v| v.to_string())
    });
    let checker = DelayChecker::new(config.delay_days);
    route_request(req, &config, &checker).await
}
