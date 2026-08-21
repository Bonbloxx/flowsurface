use reqwest::Response;
use std::time::{Duration, Instant};

pub trait RateLimiter: Send + Sync {
    /// Prepare for a request with given weight. Returns wait time if needed
    fn prepare_request(&mut self, weight: usize) -> Option<Duration>;

    /// Update the limiter with response data (e.g., rate limit headers)
    fn update_from_response(&mut self, response: &Response, weight: usize);

    /// Check if response indicates rate limiting and should exit
    fn should_exit_on_response(&self, response: &Response) -> bool;
}

/// Limiter for a fixed window rate
pub struct FixedWindowBucket {
    max_tokens: usize,
    available_tokens: usize,
    last_refill: Instant,
    refill_rate: Duration,
}

impl FixedWindowBucket {
    pub fn new(max_tokens: usize, refill_rate: Duration) -> Self {
        Self {
            max_tokens,
            available_tokens: max_tokens,
            last_refill: Instant::now(),
            refill_rate,
        }
    }

    fn tokens_per_second(&self) -> f64 {
        self.max_tokens as f64 / self.refill_rate.as_secs_f64().max(1e-6)
    }

    /// Continuously replenish tokens based on elapsed time instead of
    /// resetting the whole window at once. This keeps pacing smooth and lets
    /// wait estimates be proportional to the actual deficit.
    fn refill(&mut self) {
        let elapsed = Instant::now().duration_since(self.last_refill);
        let gained = (elapsed.as_secs_f64() * self.tokens_per_second()) as usize;
        if gained > 0 {
            self.available_tokens = (self.available_tokens + gained).min(self.max_tokens);
            self.last_refill = Instant::now();
        }
    }

    pub fn calculate_wait_time(&mut self, tokens: usize) -> Option<Duration> {
        self.refill();

        if self.available_tokens >= tokens {
            self.available_tokens -= tokens;
            return None;
        }

        // Estimate how long the deficit takes to replenish, plus a small
        // safety margin, rather than sleeping until the next full reset.
        let deficit = (tokens - self.available_tokens) as f64;
        let wait_secs = (deficit / self.tokens_per_second()) * 1.05 + 0.05;
        Some(Duration::from_secs_f64(wait_secs))
    }

    pub fn consume_tokens(&mut self, tokens: usize) {
        self.refill();
        self.available_tokens -= tokens.min(self.available_tokens);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DynamicLimitReason {
    HeaderRate,
    FixedWindowRate,
}

/// Limiter that can be used when source reports the rate-limit usage
///
/// Can fallback to fixed window bucket
pub struct DynamicBucket {
    max_weight: usize,
    current_used_weight: usize,
    last_updated: Instant,
    refill_rate: Duration,
    fallback_bucket: FixedWindowBucket,
}

impl DynamicBucket {
    pub fn new(max_weight: usize, refill_rate: Duration) -> Self {
        Self {
            max_weight,
            current_used_weight: 0,
            last_updated: Instant::now(),
            refill_rate,
            fallback_bucket: FixedWindowBucket::new(max_weight, refill_rate),
        }
    }

    pub fn update_weight(&mut self, new_weight: usize) {
        if new_weight > 0 {
            self.current_used_weight = new_weight;
            self.last_updated = Instant::now();
        }
    }

    pub fn prepare_request(
        &mut self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_updated);

        if elapsed <= self.refill_rate && self.current_used_weight > 0 {
            self.prepare_with_header_data(weight)
        } else {
            self.prepare_with_fallback(weight)
        }
    }

    fn prepare_with_header_data(
        &self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        // The server reports cumulative used weight for the current window.
        // Assume it decays linearly since the last header update, but only
        // credit half of the theoretical decay: exchange windows are aligned
        // rather than continuously sliding, so optimistic decay assumptions
        // overshoot into real 429s when several fetches run concurrently.
        let period_secs = self.refill_rate.as_secs_f64().max(1.0);
        let elapsed_secs = self.last_updated.elapsed().as_secs_f64();
        let decayed = elapsed_secs / period_secs * 0.5 * self.max_weight as f64;
        let assumed_used = (self.current_used_weight as f64 - decayed).max(0.0);
        let available = self.max_weight as f64 - assumed_used;

        if available >= weight as f64 {
            return (None, None);
        }

        let deficit = weight as f64 - available;
        let per_second = self.max_weight as f64 / period_secs;
        let wait_secs = ((deficit / per_second) * 1.1 + 0.25).min(period_secs);

        (
            Some(Duration::from_secs_f64(wait_secs)),
            Some(DynamicLimitReason::HeaderRate),
        )
    }

    fn prepare_with_fallback(
        &mut self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        match self.fallback_bucket.calculate_wait_time(weight) {
            None => (None, None),
            Some(wait_time) => (Some(wait_time), Some(DynamicLimitReason::FixedWindowRate)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FixedWindowRateLimiterConfig {
    pub limit: usize,
    pub refill_rate: Duration,
    pub limiter_buffer_pct: f32,
    pub exit_status: reqwest::StatusCode,
}

impl FixedWindowRateLimiterConfig {
    pub fn new(
        limit: usize,
        refill_rate: Duration,
        limiter_buffer_pct: f32,
        exit_status: reqwest::StatusCode,
    ) -> Self {
        Self {
            limit,
            refill_rate,
            limiter_buffer_pct,
            exit_status,
        }
    }
}

pub struct FixedWindowRateLimiter {
    bucket: FixedWindowBucket,
    exit_status: reqwest::StatusCode,
}

impl FixedWindowRateLimiter {
    pub fn new(config: FixedWindowRateLimiterConfig) -> Self {
        let keep_ratio = (1.0 - config.limiter_buffer_pct).clamp(0.0, 1.0);
        let effective_limit = (config.limit as f32 * keep_ratio) as usize;

        Self {
            bucket: FixedWindowBucket::new(effective_limit, config.refill_rate),
            exit_status: config.exit_status,
        }
    }
}

impl RateLimiter for FixedWindowRateLimiter {
    fn prepare_request(&mut self, weight: usize) -> Option<Duration> {
        self.bucket.calculate_wait_time(weight)
    }

    fn update_from_response(&mut self, _response: &reqwest::Response, weight: usize) {
        self.bucket.consume_tokens(weight);
    }

    fn should_exit_on_response(&self, response: &reqwest::Response) -> bool {
        response.status() == self.exit_status
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DynamicRateLimiterConfig {
    pub max_weight: usize,
    pub refill_rate: Duration,
    pub limiter_buffer_pct: f32,
    pub used_weight_header: &'static str,
    pub exit_status: reqwest::StatusCode,
    pub extra_exit_status: Option<reqwest::StatusCode>,
}

impl DynamicRateLimiterConfig {
    pub fn new(
        max_weight: usize,
        refill_rate: Duration,
        limiter_buffer_pct: f32,
        used_weight_header: &'static str,
        exit_status: reqwest::StatusCode,
        extra_exit_status: Option<reqwest::StatusCode>,
    ) -> Self {
        Self {
            max_weight,
            refill_rate,
            limiter_buffer_pct,
            used_weight_header,
            exit_status,
            extra_exit_status,
        }
    }
}

pub struct HeaderDynamicRateLimiter {
    bucket: DynamicBucket,
    used_weight_header: &'static str,
    exit_status: reqwest::StatusCode,
    extra_exit_status: Option<reqwest::StatusCode>,
}

impl HeaderDynamicRateLimiter {
    pub fn new(config: DynamicRateLimiterConfig) -> Self {
        let keep_ratio = (1.0 - config.limiter_buffer_pct).clamp(0.0, 1.0);
        let effective_limit = (config.max_weight as f32 * keep_ratio) as usize;

        Self {
            bucket: DynamicBucket::new(effective_limit, config.refill_rate),
            used_weight_header: config.used_weight_header,
            exit_status: config.exit_status,
            extra_exit_status: config.extra_exit_status,
        }
    }
}

impl RateLimiter for HeaderDynamicRateLimiter {
    fn prepare_request(&mut self, weight: usize) -> Option<Duration> {
        let (wait_time, _reason) = self.bucket.prepare_request(weight);
        wait_time
    }

    fn update_from_response(&mut self, response: &reqwest::Response, _weight: usize) {
        if let Some(header_value) = response
            .headers()
            .get(self.used_weight_header)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        {
            self.bucket.update_weight(header_value);
        }
    }

    fn should_exit_on_response(&self, response: &reqwest::Response) -> bool {
        let status = response.status();
        status == self.exit_status || Some(status) == self.extra_exit_status
    }
}
