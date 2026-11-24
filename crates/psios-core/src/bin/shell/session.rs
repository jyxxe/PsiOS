use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{hmac, rand};
use rustix::time::{ClockId, clock_gettime};
use std::time::Duration;

const PAIRING_LIFETIME: Duration = Duration::from_secs(120);
const IDLE_LIFETIME: Duration = Duration::from_secs(300);
const SESSION_LIFETIME: Duration = Duration::from_secs(3_600);

pub(super) fn now() -> Duration {
    let clock = clock_gettime(ClockId::Boottime);
    Duration::new(clock.tv_sec as u64, clock.tv_nsec as u32)
}

pub(super) fn secret() -> std::io::Result<String> {
    let bytes = rand::generate::<[u8; 24]>(&rand::SystemRandom::new())
        .map_err(|_| std::io::Error::other("randomness is unavailable"))?
        .expose();
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

enum Grant {
    Pairing {
        tag: hmac::Tag,
        expires: Duration,
    },
    Active {
        tag: hmac::Tag,
        expires: Duration,
        idle: Duration,
    },
    Closed,
}

pub(super) struct Session {
    key: hmac::Key,
    grant: Grant,
}

impl Session {
    pub(super) fn new(code: &str, now: Duration) -> std::io::Result<Self> {
        let key = hmac::Key::generate(hmac::HMAC_SHA256, &rand::SystemRandom::new())
            .map_err(|_| std::io::Error::other("randomness is unavailable"))?;
        let grant = Grant::Pairing {
            tag: hmac::sign(&key, code.as_bytes()),
            expires: now + PAIRING_LIFETIME,
        };
        Ok(Self { key, grant })
    }

    pub(super) fn closed(&mut self, now: Duration) -> bool {
        let expired = match &self.grant {
            Grant::Pairing { expires, .. } => now >= *expires,
            Grant::Active { expires, idle, .. } => now >= *expires || now >= *idle,
            Grant::Closed => true,
        };
        if expired {
            self.close();
        }
        expired
    }

    pub(super) fn pair(&mut self, code: &str, token: &str, now: Duration) -> bool {
        if self.closed(now) {
            return false;
        }
        let Grant::Pairing { tag, .. } = &self.grant else {
            return false;
        };
        if !self.matches(code, tag) {
            return false;
        }
        self.grant = Grant::Active {
            tag: hmac::sign(&self.key, token.as_bytes()),
            expires: now + SESSION_LIFETIME,
            idle: now + IDLE_LIFETIME,
        };
        true
    }

    pub(super) fn authorize(&mut self, token: &str, now: Duration) -> bool {
        if self.closed(now) {
            return false;
        }
        let Grant::Active { tag, .. } = &self.grant else {
            return false;
        };
        if !self.matches(token, tag) {
            return false;
        }
        if let Grant::Active { idle, .. } = &mut self.grant {
            *idle = now + IDLE_LIFETIME;
        }
        true
    }

    pub(super) fn close(&mut self) {
        self.grant = Grant::Closed;
    }

    fn matches(&self, submitted: &str, expected: &hmac::Tag) -> bool {
        submitted.len() == 32
            && hmac::verify(&self.key, submitted.as_bytes(), expected.as_ref()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_is_single_use_and_invalid_tokens_do_not_refresh_the_idle_window() {
        let at = now();
        let code = secret().unwrap();
        let token = secret().unwrap();
        let other = secret().unwrap();
        let mut session = Session::new(&code, at).unwrap();
        assert!(!session.pair(&other, &token, at));
        assert!(session.pair(&code, &token, at));
        assert!(!session.pair(&code, &other, at));
        assert!(!session.authorize(&code, at + IDLE_LIFETIME / 2));
        assert!(!session.authorize(&token, at + IDLE_LIFETIME));
    }

    #[test]
    fn pairing_and_session_have_absolute_deadlines() {
        let at = now();
        let code = secret().unwrap();
        let token = secret().unwrap();
        let mut session = Session::new(&code, at).unwrap();
        assert!(!session.pair(&code, &token, at + PAIRING_LIFETIME));
        let mut session = Session::new(&code, at).unwrap();
        assert!(session.pair(&code, &token, at));
        for seconds in (240..3_600).step_by(240) {
            assert!(session.authorize(&token, at + Duration::from_secs(seconds)));
        }
        assert!(session.closed(at + SESSION_LIFETIME));
    }
}
