use std::collections::{HashMap, HashSet};
use std::time::Duration;

use helix_core::syntax::LanguageServerFeature;
use helix_core::Uri;
use helix_event::{register_hook, send_blocking};
use helix_lsp::lsp::{self, Diagnostic};
use helix_lsp::LanguageServerId;
use helix_view::document::Mode;
use helix_view::events::{DiagnosticsDidChange, DocumentDidChange, DocumentDidOpen};
use helix_view::handlers::diagnostics::DiagnosticEvent;
use helix_view::handlers::lsp::PullDiagnosticsForLanguageServersEvent;
use helix_view::handlers::Handlers;
use helix_view::{DocumentId, Editor};
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

    let tx = handlers.pull_diagnostics_for_language_servers.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let language_server_ids: HashSet<_> = event
            .doc
            .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
            .map(|x| x.id())
            .collect();

        if !language_server_ids.is_empty() {
            send_blocking(
                &tx,
                PullDiagnosticsForLanguageServersEvent {
                    language_server_ids,
                },
            );
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
        {
            let document_id = event.doc.id();
            job::dispatch_blocking(move |editor, _| {
                let Some(doc) = editor.document_mut(document_id) else {
                    return;
                };

                let language_servers =
                    doc.language_servers_with_feature(LanguageServerFeature::PullDiagnostics);

                for language_server in language_servers {
                    pull_diagnostics_for_document(doc, language_server);
                }
            })
        }

        Ok(())
    });
}

#[derive(Debug)]
pub(super) struct PullDiagnosticsForLanguageServersHandler {
    language_server_ids: HashSet<LanguageServerId>,
}

impl PullDiagnosticsForLanguageServersHandler {
    pub fn new() -> PullDiagnosticsForLanguageServersHandler {
        PullDiagnosticsForLanguageServersHandler {
            language_server_ids: [].into(),
        }
    }
}

impl helix_event::AsyncHook for PullDiagnosticsForLanguageServersHandler {
    type Event = PullDiagnosticsForLanguageServersEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        self.language_server_ids = event.language_server_ids;
        Some(Instant::now() + Duration::from_millis(120))
    }

    fn finish_debounce(&mut self) {
        let language_servers = self.language_server_ids.clone();
        job::dispatch_blocking(move |editor, _| {
            pull_diagnostic_for_language_servers(editor, language_servers)
        })
    }
}

fn pull_diagnostic_for_language_servers(
    editor: &mut Editor,
    language_server_ids: HashSet<LanguageServerId>,
) {
    let document_ids: Vec<_> = editor.documents().map(|x| x.id()).collect();

    for document_id in document_ids {
        let Some(doc) = editor.document_mut(document_id) else {
            return;
        };

        let language_servers = doc
            .language_servers()
            .filter(|x| language_server_ids.contains(&x.id()));

        for language_server in language_servers {
            pull_diagnostics_for_document(doc, language_server);
        }
    }
}

fn pull_diagnostics_for_document(doc: &helix_view::Document, language_server: &helix_lsp::Client) {
    let Some(future) = language_server
        .text_document_diagnostic(doc.identifier(), doc.previous_diagnostic_id.clone())
    else {
        return;
    };

    let Some(uri) = doc.uri() else {
        return;
    };

    let server_id = language_server.id();
    let document_id = doc.id();

    tokio::spawn(async move {
        match future.await {
            Ok(res) => {
                job::dispatch(move |editor, _| {
                    let response = match serde_json::from_value(res) {
                        Ok(result) => result,
                        Err(_) => return,
                    };

                    handle_pull_diagnostics_response(editor, response, server_id, uri, document_id)
                })
                .await
            }
            Err(err) => log::error!("Pull diagnostic request failed: {err}"),
        }
    });
}

fn handle_pull_diagnostics_response(
    editor: &mut Editor,
    response: lsp::DocumentDiagnosticReport,
    server_id: LanguageServerId,
    uri: Uri,
    document_id: DocumentId,
) {
    let Some(doc) = editor.document_mut(document_id) else {
        return;
    };

    match response {
        lsp::DocumentDiagnosticReport::Full(report) => {
            // Original file diagnostic
            add_diagnostics_to_editor(
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

fn add_diagnostics_to_editor(
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
    document_id: DocumentId,
    report: Option<HashMap<lsp::Url, lsp::DocumentDiagnosticReportKind>>,
    server_id: LanguageServerId,
) {
    for (url, report) in report.into_iter().flatten() {
        match report {
            lsp::DocumentDiagnosticReportKind::Full(report) => {
                let Ok(uri) = Uri::try_from(url) else {
                    return;
                };

                add_diagnostics_to_editor(editor, uri, report.items, report.result_id, server_id);
            }
            lsp::DocumentDiagnosticReportKind::Unchanged(report) => {
                let Some(doc) = editor.document_mut(document_id) else {
                    return;
                };
                doc.previous_diagnostic_id = Some(report.result_id);
            }
        }
    }
}
