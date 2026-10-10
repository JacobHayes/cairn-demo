//! Restores authenticated identity on each async span entry, after Sentry selects its hub.
use sentry::protocol::{SpanStatus, User};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

pub(super) struct RequestContext;

#[derive(Default)]
struct Fields {
    user: User,
    conversation: Option<String>,
    agent: Option<String>,
    status: Option<SpanStatus>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "gen_ai.conversation.id" => self.conversation = Some(value.to_owned()),
            "gen_ai.agent.name" => self.agent = Some(value.to_owned()),
            "user.id" => self.user.id = Some(value.to_owned()),
            "user.username" => self.user.username = Some(value.to_owned()),
            "user.email" => self.user.email = Some(value.to_owned()),
            "sentry.status" => {
                self.status = Some(match value {
                    "ok" => SpanStatus::Ok,
                    "deadline_exceeded" => SpanStatus::DeadlineExceeded,
                    _ => SpanStatus::InternalError,
                });
            }
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RequestContext {
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(fields);
        }
    }
    fn on_enter(&self, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        for ancestor in span.scope() {
            if let Some(fields) = ancestor.extensions().get::<Fields>() {
                sentry::configure_scope(|scope| {
                    if fields.user.id.is_some() {
                        scope.set_user(Some(fields.user.clone()));
                    }
                    if let Some(active) = scope.get_span() {
                        if let Some(conversation) = &fields.conversation {
                            active.set_data("gen_ai.conversation.id", conversation.clone().into());
                        }
                        if let Some(agent) = &fields.agent {
                            active.set_data("gen_ai.agent.name", agent.clone().into());
                        }
                    }
                });
            }
        }
    }
    fn on_record(&self, _id: &Id, values: &Record<'_>, _context: Context<'_, S>) {
        let mut fields = Fields::default();
        values.record(&mut fields);
        if let Some(status) = fields.status {
            sentry::configure_scope(|scope| {
                if let Some(span) = scope.get_span() {
                    span.set_status(status);
                }
            });
        }
    }
}
