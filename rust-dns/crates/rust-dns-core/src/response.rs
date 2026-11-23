// SPDX-License-Identifier: GPL-3.0-only
//! Response modifiers: TTL clamp and HTTPS/SVCB rejection.

use async_trait::async_trait;
use hickory_proto::rr::{Record, RecordType};

use crate::Executable;
use crate::context::QueryContext;
use crate::error::Result;

/// Post-processing applied to the response produced earlier in the sequence.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResponseModifier {
    /// Lower bound for record TTLs; `0` disables the lower clamp.
    pub minimal_ttl: u32,
    /// Upper bound for record TTLs; `0` disables the upper clamp.
    pub maximum_ttl: u32,
    /// Remove HTTPS (type 65) / SVCB (type 64) records from the response.
    pub reject_type65: bool,
}

impl ResponseModifier {
    pub fn apply(&self, ctx: &mut QueryContext) -> Result<()> {
        let Some(response) = ctx.response.as_mut() else {
            return Ok(());
        };

        if self.minimal_ttl > 0 || self.maximum_ttl > 0 {
            for record in response
                .answers
                .iter_mut()
                .chain(response.authorities.iter_mut())
                .chain(response.additionals.iter_mut())
            {
                record.ttl = clamp_ttl(record.ttl, self.minimal_ttl, self.maximum_ttl);
            }
        }

        if self.reject_type65 {
            for section in [
                &mut response.answers,
                &mut response.authorities,
                &mut response.additionals,
            ] {
                section.retain(|record| !is_https_like(record));
            }
        }

        Ok(())
    }
}

fn clamp_ttl(ttl: u32, minimum: u32, maximum: u32) -> u32 {
    let mut ttl = ttl;
    if minimum > 0 && ttl < minimum {
        ttl = minimum;
    }
    if maximum > 0 && ttl > maximum {
        ttl = maximum;
    }
    ttl
}

fn is_https_like(record: &Record) -> bool {
    matches!(record.record_type(), RecordType::HTTPS | RecordType::SVCB)
}

#[async_trait]
impl Executable for ResponseModifier {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        self.apply(ctx)
    }
}
