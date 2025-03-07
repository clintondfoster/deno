// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::deno_exe_path;
use crate::npm_registry_url;
use crate::TestContext;
use crate::TestContextBuilder;

use super::new_deno_dir;
use super::TempDir;

use anyhow::Result;
use lazy_static::lazy_static;
use lsp_types::ClientCapabilities;
use lsp_types::ClientInfo;
use lsp_types::CodeActionCapabilityResolveSupport;
use lsp_types::CodeActionClientCapabilities;
use lsp_types::CodeActionKindLiteralSupport;
use lsp_types::CodeActionLiteralSupport;
use lsp_types::CompletionClientCapabilities;
use lsp_types::CompletionItemCapability;
use lsp_types::FoldingRangeClientCapabilities;
use lsp_types::InitializeParams;
use lsp_types::TextDocumentClientCapabilities;
use lsp_types::TextDocumentSyncClientCapabilities;
use lsp_types::Url;
use lsp_types::WorkspaceClientCapabilities;
use parking_lot::Condvar;
use parking_lot::Mutex;
use regex::Regex;
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::to_value;
use serde_json::Value;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

lazy_static! {
  static ref CONTENT_TYPE_REG: Regex =
    Regex::new(r"(?i)^content-length:\s+(\d+)").unwrap();
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LspResponseError {
  code: i32,
  message: String,
  data: Option<Value>,
}

#[derive(Clone, Debug)]
pub enum LspMessage {
  Notification(String, Option<Value>),
  Request(u64, String, Option<Value>),
  Response(u64, Option<Value>, Option<LspResponseError>),
}

impl<'a> From<&'a [u8]> for LspMessage {
  fn from(s: &'a [u8]) -> Self {
    let value: Value = serde_json::from_slice(s).unwrap();
    let obj = value.as_object().unwrap();
    if obj.contains_key("id") && obj.contains_key("method") {
      let id = obj.get("id").unwrap().as_u64().unwrap();
      let method = obj.get("method").unwrap().as_str().unwrap().to_string();
      Self::Request(id, method, obj.get("params").cloned())
    } else if obj.contains_key("id") {
      let id = obj.get("id").unwrap().as_u64().unwrap();
      let maybe_error: Option<LspResponseError> = obj
        .get("error")
        .map(|v| serde_json::from_value(v.clone()).unwrap());
      Self::Response(id, obj.get("result").cloned(), maybe_error)
    } else {
      assert!(obj.contains_key("method"));
      let method = obj.get("method").unwrap().as_str().unwrap().to_string();
      Self::Notification(method, obj.get("params").cloned())
    }
  }
}

fn read_message<R>(reader: &mut R) -> Result<Option<Vec<u8>>>
where
  R: io::Read + io::BufRead,
{
  let mut content_length = 0_usize;
  loop {
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
      return Ok(None);
    }
    if let Some(captures) = CONTENT_TYPE_REG.captures(&buf) {
      let content_length_match = captures
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing capture"))?;
      content_length = content_length_match.as_str().parse::<usize>()?;
    }
    if &buf == "\r\n" {
      break;
    }
  }

  let mut msg_buf = vec![0_u8; content_length];
  reader.read_exact(&mut msg_buf)?;
  Ok(Some(msg_buf))
}

struct LspStdoutReader {
  pending_messages: Arc<(Mutex<Vec<LspMessage>>, Condvar)>,
  read_messages: Vec<LspMessage>,
}

impl LspStdoutReader {
  pub fn new(mut buf_reader: io::BufReader<ChildStdout>) -> Self {
    let messages: Arc<(Mutex<Vec<LspMessage>>, Condvar)> = Default::default();
    std::thread::spawn({
      let messages = messages.clone();
      move || {
        while let Ok(Some(msg_buf)) = read_message(&mut buf_reader) {
          let msg = LspMessage::from(msg_buf.as_slice());
          let cvar = &messages.1;
          {
            let mut messages = messages.0.lock();
            messages.push(msg);
          }
          cvar.notify_all();
        }
      }
    });

    LspStdoutReader {
      pending_messages: messages,
      read_messages: Vec::new(),
    }
  }

  pub fn pending_len(&self) -> usize {
    self.pending_messages.0.lock().len()
  }

  pub fn had_message(&self, is_match: impl Fn(&LspMessage) -> bool) -> bool {
    self.read_messages.iter().any(&is_match)
      || self.pending_messages.0.lock().iter().any(&is_match)
  }

  pub fn read_message<R>(
    &mut self,
    mut get_match: impl FnMut(&LspMessage) -> Option<R>,
  ) -> R {
    let (msg_queue, cvar) = &*self.pending_messages;
    let mut msg_queue = msg_queue.lock();
    loop {
      for i in 0..msg_queue.len() {
        let msg = &msg_queue[i];
        if let Some(result) = get_match(msg) {
          let msg = msg_queue.remove(i);
          self.read_messages.push(msg);
          return result;
        }
      }
      cvar.wait(&mut msg_queue);
    }
  }
}

pub struct InitializeParamsBuilder {
  params: InitializeParams,
}

impl InitializeParamsBuilder {
  #[allow(clippy::new_without_default)]
  pub fn new() -> Self {
    Self {
      params: InitializeParams {
        process_id: None,
        client_info: Some(ClientInfo {
          name: "test-harness".to_string(),
          version: Some("1.0.0".to_string()),
        }),
        root_uri: None,
        initialization_options: Some(json!({
          "enable": true,
          "cache": null,
          "certificateStores": null,
          "codeLens": {
            "implementations": true,
            "references": true,
            "test": true
          },
          "config": null,
          "importMap": null,
          "lint": true,
          "suggest": {
            "autoImports": true,
            "completeFunctionCalls": false,
            "names": true,
            "paths": true,
            "imports": {
              "hosts": {}
            }
          },
          "testing": {
            "args": [
              "--allow-all"
            ],
            "enable": true
          },
          "tlsCertificate": null,
          "unsafelyIgnoreCertificateErrors": null,
          "unstable": false
        })),
        capabilities: ClientCapabilities {
          text_document: Some(TextDocumentClientCapabilities {
            code_action: Some(CodeActionClientCapabilities {
              code_action_literal_support: Some(CodeActionLiteralSupport {
                code_action_kind: CodeActionKindLiteralSupport {
                  value_set: vec![
                    "quickfix".to_string(),
                    "refactor".to_string(),
                  ],
                },
              }),
              is_preferred_support: Some(true),
              data_support: Some(true),
              disabled_support: Some(true),
              resolve_support: Some(CodeActionCapabilityResolveSupport {
                properties: vec!["edit".to_string()],
              }),
              ..Default::default()
            }),
            completion: Some(CompletionClientCapabilities {
              completion_item: Some(CompletionItemCapability {
                snippet_support: Some(true),
                ..Default::default()
              }),
              ..Default::default()
            }),
            folding_range: Some(FoldingRangeClientCapabilities {
              line_folding_only: Some(true),
              ..Default::default()
            }),
            synchronization: Some(TextDocumentSyncClientCapabilities {
              dynamic_registration: Some(true),
              will_save: Some(true),
              will_save_wait_until: Some(true),
              did_save: Some(true),
            }),
            ..Default::default()
          }),
          workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            ..Default::default()
          }),
          experimental: Some(json!({
            "testingApi": true
          })),
          ..Default::default()
        },
        ..Default::default()
      },
    }
  }

  pub fn set_maybe_root_uri(&mut self, value: Option<Url>) -> &mut Self {
    self.params.root_uri = value;
    self
  }

  pub fn set_root_uri(&mut self, value: Url) -> &mut Self {
    self.set_maybe_root_uri(Some(value))
  }

  pub fn set_workspace_folders(
    &mut self,
    folders: Vec<lsp_types::WorkspaceFolder>,
  ) -> &mut Self {
    self.params.workspace_folders = Some(folders);
    self
  }

  pub fn enable_inlay_hints(&mut self) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert(
      "inlayHints".to_string(),
      json!({
        "parameterNames": {
          "enabled": "all"
        },
        "parameterTypes": {
          "enabled": true
        },
        "variableTypes": {
          "enabled": true
        },
        "propertyDeclarationTypes": {
          "enabled": true
        },
        "functionLikeReturnTypes": {
          "enabled": true
        },
        "enumMemberValues": {
          "enabled": true
        }
      }),
    );
    self
  }

  pub fn disable_testing_api(&mut self) -> &mut Self {
    let obj = self
      .params
      .capabilities
      .experimental
      .as_mut()
      .unwrap()
      .as_object_mut()
      .unwrap();
    obj.insert("testingApi".to_string(), false.into());
    let options = self.initialization_options_mut();
    options.remove("testing");
    self
  }

  pub fn set_cache(&mut self, value: impl AsRef<str>) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("cache".to_string(), value.as_ref().to_string().into());
    self
  }

  pub fn set_code_lens(
    &mut self,
    value: Option<serde_json::Value>,
  ) -> &mut Self {
    let options = self.initialization_options_mut();
    if let Some(value) = value {
      options.insert("codeLens".to_string(), value);
    } else {
      options.remove("codeLens");
    }
    self
  }

  pub fn set_config(&mut self, value: impl AsRef<str>) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("config".to_string(), value.as_ref().to_string().into());
    self
  }

  pub fn set_enable_paths(&mut self, value: Vec<String>) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("enablePaths".to_string(), value.into());
    self
  }

  pub fn set_deno_enable(&mut self, value: bool) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("enable".to_string(), value.into());
    self
  }

  pub fn set_import_map(&mut self, value: impl AsRef<str>) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("importMap".to_string(), value.as_ref().to_string().into());
    self
  }

  pub fn set_tls_certificate(&mut self, value: impl AsRef<str>) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert(
      "tlsCertificate".to_string(),
      value.as_ref().to_string().into(),
    );
    self
  }

  pub fn set_unstable(&mut self, value: bool) -> &mut Self {
    let options = self.initialization_options_mut();
    options.insert("unstable".to_string(), value.into());
    self
  }

  pub fn add_test_server_suggestions(&mut self) -> &mut Self {
    self.set_suggest_imports_hosts(vec![(
      "http://localhost:4545/".to_string(),
      true,
    )])
  }

  pub fn set_suggest_imports_hosts(
    &mut self,
    values: Vec<(String, bool)>,
  ) -> &mut Self {
    let options = self.initialization_options_mut();
    let suggest = options.get_mut("suggest").unwrap().as_object_mut().unwrap();
    let imports = suggest.get_mut("imports").unwrap().as_object_mut().unwrap();
    let hosts = imports.get_mut("hosts").unwrap().as_object_mut().unwrap();
    hosts.clear();
    for (key, value) in values {
      hosts.insert(key, value.into());
    }
    self
  }

  pub fn with_capabilities(
    &mut self,
    mut action: impl FnMut(&mut ClientCapabilities),
  ) -> &mut Self {
    action(&mut self.params.capabilities);
    self
  }

  fn initialization_options_mut(
    &mut self,
  ) -> &mut serde_json::Map<String, serde_json::Value> {
    let options = self.params.initialization_options.as_mut().unwrap();
    options.as_object_mut().unwrap()
  }

  pub fn build(&self) -> InitializeParams {
    self.params.clone()
  }
}

pub struct LspClientBuilder {
  print_stderr: bool,
  deno_exe: PathBuf,
  context: Option<TestContext>,
}

impl LspClientBuilder {
  #[allow(clippy::new_without_default)]
  pub fn new() -> Self {
    Self {
      print_stderr: false,
      deno_exe: deno_exe_path(),
      context: None,
    }
  }

  pub fn deno_exe(&mut self, exe_path: impl AsRef<Path>) -> &mut Self {
    self.deno_exe = exe_path.as_ref().to_path_buf();
    self
  }

  pub fn print_stderr(&mut self) -> &mut Self {
    self.print_stderr = true;
    self
  }

  pub fn set_test_context(&mut self, test_context: &TestContext) -> &mut Self {
    self.context = Some(test_context.clone());
    self
  }

  pub fn build(&self) -> LspClient {
    self.build_result().unwrap()
  }

  pub fn build_result(&self) -> Result<LspClient> {
    let deno_dir = new_deno_dir();
    let mut command = Command::new(&self.deno_exe);
    command
      .env("DENO_DIR", deno_dir.path())
      .env("NPM_CONFIG_REGISTRY", npm_registry_url())
      .arg("lsp")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped());
    if !self.print_stderr {
      command.stderr(Stdio::null());
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();
    let buf_reader = io::BufReader::new(stdout);
    let reader = LspStdoutReader::new(buf_reader);

    let stdin = child.stdin.take().unwrap();
    let writer = io::BufWriter::new(stdin);

    Ok(LspClient {
      child,
      reader,
      request_id: 1,
      start: Instant::now(),
      context: self
        .context
        .clone()
        .unwrap_or_else(|| TestContextBuilder::new().build()),
      writer,
      deno_dir,
    })
  }
}

pub struct LspClient {
  child: Child,
  reader: LspStdoutReader,
  request_id: u64,
  start: Instant,
  writer: io::BufWriter<ChildStdin>,
  deno_dir: TempDir,
  context: TestContext,
}

impl Drop for LspClient {
  fn drop(&mut self) {
    match self.child.try_wait() {
      Ok(None) => {
        self.child.kill().unwrap();
        let _ = self.child.wait();
      }
      Ok(Some(status)) => panic!("deno lsp exited unexpectedly {status}"),
      Err(e) => panic!("pebble error: {e}"),
    }
  }
}

fn notification_result<R>(
  method: String,
  maybe_params: Option<Value>,
) -> Result<(String, Option<R>)>
where
  R: de::DeserializeOwned,
{
  let maybe_params = match maybe_params {
    Some(params) => {
      Some(serde_json::from_value(params.clone()).map_err(|err| {
        anyhow::anyhow!(
          "Could not deserialize message '{}': {}\n\n{:?}",
          method,
          err,
          params
        )
      })?)
    }
    None => None,
  };
  Ok((method, maybe_params))
}

fn request_result<R>(
  id: u64,
  method: String,
  maybe_params: Option<Value>,
) -> Result<(u64, String, Option<R>)>
where
  R: de::DeserializeOwned,
{
  let maybe_params = match maybe_params {
    Some(params) => Some(serde_json::from_value(params)?),
    None => None,
  };
  Ok((id, method, maybe_params))
}

fn response_result<R>(
  maybe_result: Option<Value>,
  maybe_error: Option<LspResponseError>,
) -> Result<(Option<R>, Option<LspResponseError>)>
where
  R: de::DeserializeOwned,
{
  let maybe_result = match maybe_result {
    Some(result) => Some(serde_json::from_value(result)?),
    None => None,
  };
  Ok((maybe_result, maybe_error))
}

impl LspClient {
  pub fn deno_dir(&self) -> &TempDir {
    &self.deno_dir
  }

  pub fn duration(&self) -> Duration {
    self.start.elapsed()
  }

  pub fn queue_is_empty(&self) -> bool {
    self.reader.pending_len() == 0
  }

  pub fn queue_len(&self) -> usize {
    self.reader.pending_len()
  }

  pub fn initialize_default(&mut self) {
    self.initialize(|_| {})
  }

  pub fn initialize(
    &mut self,
    do_build: impl Fn(&mut InitializeParamsBuilder),
  ) {
    let mut builder = InitializeParamsBuilder::new();
    builder.set_root_uri(self.context.deno_dir().uri());
    do_build(&mut builder);
    self
      .write_request::<_, _, Value>("initialize", builder.build())
      .unwrap();
    self.write_notification("initialized", json!({})).unwrap();
  }

  pub fn shutdown(&mut self) {
    self
      .write_request::<_, _, Value>("shutdown", json!(null))
      .unwrap();
    self.write_notification("exit", json!(null)).unwrap();
  }

  // it's flaky to assert for a notification because a notification
  // might arrive a little later, so only provide a method for asserting
  // that there is no notification
  pub fn assert_no_notification(&mut self, searching_method: &str) {
    assert!(!self.reader.had_message(|message| match message {
      LspMessage::Notification(method, _) => method == searching_method,
      _ => false,
    }))
  }

  pub fn read_notification<R>(&mut self) -> Result<(String, Option<R>)>
  where
    R: de::DeserializeOwned,
  {
    self.reader.read_message(|msg| match msg {
      LspMessage::Notification(method, maybe_params) => Some(
        notification_result(method.to_owned(), maybe_params.to_owned()),
      ),
      _ => None,
    })
  }

  pub fn read_request<R>(&mut self) -> Result<(u64, String, Option<R>)>
  where
    R: de::DeserializeOwned,
  {
    self.reader.read_message(|msg| match msg {
      LspMessage::Request(id, method, maybe_params) => Some(request_result(
        *id,
        method.to_owned(),
        maybe_params.to_owned(),
      )),
      _ => None,
    })
  }

  fn write(&mut self, value: Value) -> Result<()> {
    let value_str = value.to_string();
    let msg = format!(
      "Content-Length: {}\r\n\r\n{}",
      value_str.as_bytes().len(),
      value_str
    );
    self.writer.write_all(msg.as_bytes())?;
    self.writer.flush()?;
    Ok(())
  }

  pub fn write_request<S, V, R>(
    &mut self,
    method: S,
    params: V,
  ) -> Result<(Option<R>, Option<LspResponseError>)>
  where
    S: AsRef<str>,
    V: Serialize,
    R: de::DeserializeOwned,
  {
    let value = if to_value(&params).unwrap().is_null() {
      json!({
        "jsonrpc": "2.0",
        "id": self.request_id,
        "method": method.as_ref(),
      })
    } else {
      json!({
        "jsonrpc": "2.0",
        "id": self.request_id,
        "method": method.as_ref(),
        "params": params,
      })
    };
    self.write(value)?;

    self.reader.read_message(|msg| match msg {
      LspMessage::Response(id, maybe_result, maybe_error) => {
        assert_eq!(*id, self.request_id);
        self.request_id += 1;
        Some(response_result(
          maybe_result.to_owned(),
          maybe_error.to_owned(),
        ))
      }
      _ => None,
    })
  }

  pub fn write_response<V>(&mut self, id: u64, result: V) -> Result<()>
  where
    V: Serialize,
  {
    let value = json!({
      "jsonrpc": "2.0",
      "id": id,
      "result": result
    });
    self.write(value)
  }

  pub fn write_notification<S, V>(&mut self, method: S, params: V) -> Result<()>
  where
    S: AsRef<str>,
    V: Serialize,
  {
    let value = json!({
      "jsonrpc": "2.0",
      "method": method.as_ref(),
      "params": params,
    });
    self.write(value)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_read_message() {
    let msg1 = b"content-length: 11\r\n\r\nhello world";
    let mut reader1 = std::io::Cursor::new(msg1);
    assert_eq!(read_message(&mut reader1).unwrap().unwrap(), b"hello world");

    let msg2 = b"content-length: 5\r\n\r\nhello world";
    let mut reader2 = std::io::Cursor::new(msg2);
    assert_eq!(read_message(&mut reader2).unwrap().unwrap(), b"hello");
  }

  #[test]
  #[should_panic(expected = "failed to fill whole buffer")]
  fn test_invalid_read_message() {
    let msg1 = b"content-length: 12\r\n\r\nhello world";
    let mut reader1 = std::io::Cursor::new(msg1);
    read_message(&mut reader1).unwrap();
  }
}
