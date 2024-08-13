use std::collections::HashMap;
use std::time::Duration;

use helix_core::syntax::LanguageServerFeature;
use helix_core::Uri;
use helix_event::{register_hook, send_blocking};
use helix_lsp::lsp::{self, Diagnostic};
use helix_lsp::LanguageServerId;
use helix_view::document::Mode;
use helix_view::events::{DiagnosticsDidChange, DocumentDidChange};
use helix_view::handlers::diagnostics::DiagnosticEvent;
use helix_view::handlers::lsp::PullDiagnosticsEvent;
use helix_view::handlers::Handlers;
use helix_view::Editor;
use tokio::time::Instant;

use crate::events::OnModeSwitch;
use crate::job;

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if event.editor.mode != Mode::Insert {
            for (view, _) in event.editor.tree.views_mut() {
                send_blocking(&view.diagnostics_handler.events, DiagnosticEvent::Refresh)
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        for (view, _) in event.cx.editor.tree.views_mut() {
            view.diagnostics_handler.active = event.new_mode != Mode::Insert;
        }
        Ok(())
    });

    let tx = handlers.pull_diagnostics.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
        {
            let language_server_ids = event
                .doc
                .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
                .map(|x| x.id())
                .collect();

            send_blocking(
                &tx,
                PullDiagnosticsEvent {
                    language_server_ids,
                },
            );
        }
        Ok(())
    });
}

const TIMEOUT: u64 = 120;

#[derive(Debug)]
pub(super) struct PullDiagnosticsHandler {
    language_server_ids: Vec<LanguageServerId>,
}

impl PullDiagnosticsHandler {
    pub fn new() -> PullDiagnosticsHandler {
        PullDiagnosticsHandler {
            language_server_ids: vec![],
        }
    }
}

impl helix_event::AsyncHook for PullDiagnosticsHandler {
    type Event = PullDiagnosticsEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        self.language_server_ids = event.language_server_ids;
        Some(Instant::now() + Duration::from_millis(TIMEOUT))
    }

    fn finish_debounce(&mut self) {
        let language_servers = self.language_server_ids.clone();
        job::dispatch_blocking(move |editor, _| {
            pull_diagnostic_for_document(
                editor,
                language_servers,
                editor.documents().map(|x| x.id()).collect(),
            )
        })
    }
}

fn pull_diagnostic_for_document(
    editor: &mut Editor,
    language_server_ids: Vec<LanguageServerId>,
    document_ids: Vec<helix_view::DocumentId>,
) {
    for document_id in document_ids.clone() {
        let doc = doc_mut!(editor, &document_id);
        let language_servers = doc
            .language_servers()
            .filter(|x| language_server_ids.contains(&x.id()));

        for language_server in language_servers {
            let Some(future) = language_server
                .text_document_diagnostic(doc.identifier(), doc.previous_diagnostic_id.clone())
            else {
                return;
            };

            let Some(uri) = doc.uri() else {
                return;
            };

            let server_id = language_server.id();

            tokio::spawn(async move {
                match future.await {
                    Ok(res) => {
                        job::dispatch(move |editor, _| {
                            log::error!("{}", res);

                            let parsed_response: Option<lsp::DocumentDiagnosticReport> =
                                match serde_json::from_value(res) {
                                    Ok(result) => Some(result),
                                    Err(_) => None,
                                };

                            let Some(response) = parsed_response else {
                                return;
                            };

                            show_pull_diagnostics(editor, response, server_id, uri, &document_id)
                        })
                        .await
                    }
                    Err(err) => log::error!("signature help request failed: {err}"),
                }
            });
        }
    }
}

fn show_pull_diagnostics(
    editor: &mut Editor,
    response: lsp::DocumentDiagnosticReport,
    server_id: LanguageServerId,
    uri: Uri,
    document_id: &helix_view::DocumentId,
) {
    let doc = doc_mut!(editor, document_id);
    match response {
        lsp::DocumentDiagnosticReport::Full(report) => {
            // Original file diagnostic
            parse_diagnostic(
                editor,
                uri,
                report.full_document_diagnostic_report.items,
                report.full_document_diagnostic_report.result_id,
                server_id,
            );

            // Related files diagnostic
            handle_document_diagnostic_report_kind(
                editor,
                document_id,
                report.related_documents,
                server_id,
            );
        }
        lsp::DocumentDiagnosticReport::Unchanged(report) => {
            doc.previous_diagnostic_id =
                Some(report.unchanged_document_diagnostic_report.result_id);

            handle_document_diagnostic_report_kind(
                editor,
                document_id,
                report.related_documents,
                server_id,
            );
        }
    }
}

fn parse_diagnostic(
    editor: &mut Editor,
    uri: Uri,
    report: Vec<lsp::Diagnostic>,
    result_id: Option<String>,
    server_id: LanguageServerId,
) {
    let diagnostics: Vec<(Diagnostic, LanguageServerId)> =
        report.into_iter().map(|d| (d, server_id)).collect();

    editor.add_diagnostics(diagnostics, server_id, uri, None, result_id);
}

fn handle_document_diagnostic_report_kind(
    editor: &mut Editor,
    document_id: &helix_view::DocumentId,
    report: Option<HashMap<lsp::Url, lsp::DocumentDiagnosticReportKind>>,
    server_id: LanguageServerId,
) {
    for (url, report) in report.into_iter().flatten() {
        match report {
            lsp::DocumentDiagnosticReportKind::Full(report) => {
                let Ok(uri) = Uri::try_from(url) else {
                    return;
                };

                parse_diagnostic(editor, uri, report.items, report.result_id, server_id);
            }
            lsp::DocumentDiagnosticReportKind::Unchanged(report) => {
                let doc = doc_mut!(editor, &document_id);
                doc.previous_diagnostic_id = Some(report.result_id);
            }
        }
    }
}
