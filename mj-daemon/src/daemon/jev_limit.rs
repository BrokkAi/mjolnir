use super::RuntimeState;
use mj_core::jev::{self, RateLimitKind};
use tokio_util::sync::CancellationToken;

pub(super) async fn publish_notices(state: &RuntimeState, cancellation: CancellationToken) {
    let mut revisions = state.revisions();
    let mut local = jev::rate_limit_changes();
    let mut shared_reported = false;
    let mut fastino_reported = false;
    loop {
        revisions.borrow_and_update();
        local.borrow_and_update();
        let own = jev::current_rate_limit();
        let (mut shared, mut fastino) = (
            own == Some(RateLimitKind::SharedProxy),
            own == Some(RateLimitKind::Fastino),
        );
        {
            let owner = state.owner();
            for view in owner.sessions.values().filter(|view| view.connected) {
                match view
                    .operational
                    .as_ref()
                    .and_then(|state| state.jev_rate_limit)
                {
                    Some(RateLimitKind::SharedProxy) => shared = true,
                    Some(RateLimitKind::Fastino) => fastino = true,
                    None => {}
                }
            }
        }
        if shared && !shared_reported {
            shared_reported = true;
            state.push_public_notice(
                "",
                "The shared Jev proxy rate limit was reached. Turn, question, handback, or GitHub relevance checks may fall back. Add your Fastino key in Settings → Jev to use your own quota.",
            );
        }
        if fastino && !fastino_reported {
            fastino_reported = true;
            state.push_public_notice(
                "",
                "Fastino rate-limited Jev. Turn, question, handback, or GitHub relevance checks may fall back. Wait for the limit to reset or check your Fastino quota if you supplied a key.",
            );
        }
        if shared_reported && fastino_reported {
            return;
        }
        tokio::select! {
            () = cancellation.cancelled() => return,
            changed = revisions.changed() => if changed.is_err() { return },
            _ = local.changed() => {}
        }
    }
}
