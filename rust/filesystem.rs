use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Error, RequestCancellation};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use walkdir::WalkDir;

const MAX_FILE_CONTENT_BYTES: usize = 4_000_000;
const MAX_FILE_SCAN_BYTES: usize = 128_000_000;
const MAX_FILE_PATH_LENGTH: usize = 16_384;
const MAX_CONTEXT_BYTES: u64 = 3 * 1024 * 1024;
const MAX_CONTEXT_QUERY_LENGTH: usize = 256;
const MAX_CONTEXT_RESULTS: usize = 24;
const MAX_CONTEXT_SCAN_ENTRIES: usize = 50_000;
const MAX_CONTEXT_DEPTH: usize = 32;

#[derive(Clone)]
pub struct WorkspaceFileSystem {
    roots: Arc<Vec<WorkspaceRoot>>,
    read_only: bool,
}

#[derive(Clone)]
struct WorkspaceRoot {
    lexical: PathBuf,
    canonical: PathBuf,
}

impl WorkspaceFileSystem {
    pub fn new(root: &Path, read_only: bool, additional_roots: &[PathBuf]) -> Result<Self, Error> {
        let mut roots = Vec::new();
        for candidate in std::iter::once(root).chain(additional_roots.iter().map(PathBuf::as_path))
        {
            let lexical = lexical_absolute(candidate)?;
            let canonical = std::fs::canonicalize(&lexical).map_err(|error| {
                Error::invalid_params().data(format!(
                    "workspace root cannot be resolved ({}): {error}",
                    lexical.display()
                ))
            })?;
            if !roots
                .iter()
                .any(|root: &WorkspaceRoot| root.canonical == canonical)
            {
                roots.push(WorkspaceRoot { lexical, canonical });
            }
        }
        Ok(Self {
            roots: Arc::new(roots),
            read_only,
        })
    }

    #[cfg(test)]
    pub async fn read(&self, request: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        if request.line == Some(0) {
            return Err(Error::invalid_params().data("ACP file read line must be 1-based"));
        }
        self.read_validated(request).await
    }

    pub async fn read_cancellable(
        &self,
        request: ReadTextFileRequest,
        cancellation: RequestCancellation,
    ) -> Result<ReadTextFileResponse, Error> {
        if request.line == Some(0) {
            return Err(Error::invalid_params().data("ACP file read line must be 1-based"));
        }
        cancellation
            .run_until_cancelled(self.read_validated(request))
            .await
    }

    async fn read_validated(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, Error> {
        let path = self.checked_existing_path(&request.path).await?;
        if request.line.is_some() || request.limit.is_some() {
            return self
                .read_range(&path, request.line.unwrap_or(1), request.limit)
                .await
                .map(ReadTextFileResponse::new);
        }

        let metadata = tokio::fs::metadata(&path).await.map_err(fs_error)?;
        if !metadata.is_file() {
            return Err(Error::invalid_params().data("ACP file read target must be a regular file"));
        }
        if metadata.len() > MAX_FILE_CONTENT_BYTES as u64 {
            return Err(Error::invalid_request().data(format!(
                "ACP file read exceeds {MAX_FILE_CONTENT_BYTES} bytes"
            )));
        }
        let mut file = tokio::fs::File::open(&path).await.map_err(fs_error)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes).await.map_err(fs_error)?;
        if bytes.len() > MAX_FILE_CONTENT_BYTES {
            return Err(Error::invalid_request().data(format!(
                "ACP file read exceeds {MAX_FILE_CONTENT_BYTES} bytes"
            )));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| Error::invalid_request().data("ACP file is not valid UTF-8 text"))?;
        Ok(ReadTextFileResponse::new(content))
    }

    #[cfg(test)]
    pub async fn write(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, Error> {
        self.write_cancellable_inner(request, None).await
    }

    pub async fn write_cancellable(
        &self,
        request: WriteTextFileRequest,
        cancellation: RequestCancellation,
    ) -> Result<WriteTextFileResponse, Error> {
        self.write_cancellable_inner(request, Some(&cancellation))
            .await
    }

    async fn write_cancellable_inner(
        &self,
        request: WriteTextFileRequest,
        cancellation: Option<&RequestCancellation>,
    ) -> Result<WriteTextFileResponse, Error> {
        if self.read_only {
            return Err(Error::invalid_request().data("attyd is running in read-only mode"));
        }
        if request.content.len() > MAX_FILE_CONTENT_BYTES {
            return Err(Error::invalid_request().data(format!(
                "ACP file write exceeds {MAX_FILE_CONTENT_BYTES} bytes"
            )));
        }
        ensure_not_cancelled(cancellation)?;
        let lexical = self.checked_lexical_path(&request.path)?;
        let target = match tokio::fs::canonicalize(&lexical).await {
            Ok(existing) => {
                self.assert_canonical_within(&existing)?;
                existing
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = lexical.parent().ok_or_else(|| {
                    Error::invalid_params().data("ACP file write target has no parent")
                })?;
                let canonical_parent = tokio::fs::canonicalize(parent).await.map_err(fs_error)?;
                self.assert_canonical_within(&canonical_parent)?;
                canonical_parent.join(lexical.file_name().ok_or_else(|| {
                    Error::invalid_params().data("ACP file write target has no file name")
                })?)
            }
            Err(error) => return Err(fs_error(error)),
        };
        // Match the Node oracle: cancellation is honored until the mutation starts.
        // Once an in-place write begins it must finish, otherwise the peer could leave
        // a previously valid file truncated or partially written.
        ensure_not_cancelled(cancellation)?;
        let mut file = tokio::fs::File::create(target).await.map_err(fs_error)?;
        file.write_all(request.content.as_bytes())
            .await
            .map_err(fs_error)?;
        file.flush().await.map_err(fs_error)?;
        Ok(WriteTextFileResponse::new())
    }

    pub async fn checked_directory(&self, path: Option<&Path>) -> Result<PathBuf, Error> {
        let lexical = match path {
            Some(path) => self.checked_lexical_path(path)?,
            None => self.roots[0].lexical.clone(),
        };
        let canonical = tokio::fs::canonicalize(&lexical).await.map_err(fs_error)?;
        self.assert_canonical_within(&canonical)?;
        let metadata = tokio::fs::metadata(&canonical).await.map_err(fs_error)?;
        if !metadata.is_dir() {
            return Err(Error::invalid_params().data("terminal cwd must be a directory"));
        }
        Ok(canonical)
    }

    pub async fn search_context(&self, query: &str) -> Result<Vec<Value>, Error> {
        if query.len() > MAX_CONTEXT_QUERY_LENGTH || query.contains('\0') {
            return Err(Error::invalid_params().data(format!(
                "context search query must be at most {MAX_CONTEXT_QUERY_LENGTH} characters without NUL bytes"
            )));
        }
        let roots = self.roots.clone();
        let query = query.trim().to_lowercase();
        tokio::task::spawn_blocking(move || {
            let terms = query
                .split(|character: char| {
                    character == '/' || character == '\\' || character.is_whitespace()
                })
                .filter(|term| !term.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            let mut candidates = Vec::new();
            let mut scanned = 0_usize;
            for root in roots.iter() {
                let root_name = root
                    .canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_else(|| root.canonical.to_str().unwrap_or("workspace"))
                    .to_string();
                let walker = WalkDir::new(&root.canonical)
                    .max_depth(MAX_CONTEXT_DEPTH)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|entry| {
                        entry.depth() == 0
                            || !entry.file_type().is_dir()
                            || !ignored_context_directory(
                                entry.file_name().to_string_lossy().as_ref(),
                            )
                    });
                for entry in walker.filter_map(Result::ok) {
                    scanned += 1;
                    if scanned > MAX_CONTEXT_SCAN_ENTRIES {
                        break;
                    }
                    if !entry.file_type().is_file() || !is_context_text_path(entry.path()) {
                        continue;
                    }
                    let relative = entry
                        .path()
                        .strip_prefix(&root.canonical)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .replace('\\', "/");
                    let Some(score) = context_match_score(&relative, &query, &terms) else {
                        continue;
                    };
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    if metadata.len() > MAX_CONTEXT_BYTES {
                        continue;
                    }
                    candidates.push((
                        score,
                        relative,
                        root_name.clone(),
                        entry.path().to_path_buf(),
                        metadata.len(),
                    ));
                }
                if scanned > MAX_CONTEXT_SCAN_ENTRIES {
                    break;
                }
            }
            candidates.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.cmp(&right.1))
                    .then_with(|| left.2.cmp(&right.2))
            });
            Ok(candidates
                .into_iter()
                .take(MAX_CONTEXT_RESULTS)
                .map(|(_, relative_path, root_name, path, size)| {
                    json!({
                        "path": path,
                        "name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
                        "relativePath": relative_path,
                        "rootName": root_name,
                        "size": size,
                    })
                })
                .collect())
        })
        .await
        .map_err(|error| Error::internal_error().data(error.to_string()))?
    }

    pub async fn read_context(&self, path: &Path) -> Result<Value, Error> {
        let target = self.checked_existing_path(path).await?;
        if !is_context_text_path(&target) {
            return Err(
                Error::invalid_params().data("workspace context is not a supported text file")
            );
        }
        let metadata = tokio::fs::metadata(&target).await.map_err(fs_error)?;
        if !metadata.is_file() || metadata.len() > MAX_CONTEXT_BYTES {
            return Err(Error::invalid_request().data(format!(
                "workspace context exceeds {MAX_CONTEXT_BYTES} bytes"
            )));
        }
        let bytes = tokio::fs::read(&target).await.map_err(fs_error)?;
        if bytes.len() as u64 > MAX_CONTEXT_BYTES || bytes.contains(&0) {
            return Err(Error::invalid_request().data("workspace context is not a text file"));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::invalid_request().data("workspace context is not valid UTF-8"))?;
        let root = self
            .roots
            .iter()
            .find(|root| target.starts_with(&root.canonical));
        let name = root
            .and_then(|root| target.strip_prefix(&root.canonical).ok())
            .unwrap_or(&target)
            .to_string_lossy()
            .to_string();
        let uri = url::Url::from_file_path(&target)
            .map_err(|_| Error::internal_error().data("failed to create file URL"))?;
        Ok(json!({
            "name": name,
            "size": text.len(),
            "block": {
                "type": "resource",
                "resource": {
                    "uri": uri.as_str(),
                    "mimeType": context_mime_type(&target),
                    "text": text,
                }
            }
        }))
    }

    async fn checked_existing_path(&self, path: &Path) -> Result<PathBuf, Error> {
        let lexical = self.checked_lexical_path(path)?;
        let canonical = tokio::fs::canonicalize(&lexical).await.map_err(fs_error)?;
        self.assert_canonical_within(&canonical)?;
        Ok(canonical)
    }

    fn checked_lexical_path(&self, path: &Path) -> Result<PathBuf, Error> {
        let path_text = path.to_string_lossy();
        if path_text.is_empty()
            || path_text.len() > MAX_FILE_PATH_LENGTH
            || path_text.contains('\0')
            || !path.is_absolute()
        {
            return Err(Error::invalid_params().data(format!(
                "ACP filesystem path must be absolute, non-empty, and at most {MAX_FILE_PATH_LENGTH} bytes"
            )));
        }
        let lexical = normalize(path);
        if self
            .roots
            .iter()
            .any(|root| lexical.starts_with(&root.lexical) || lexical.starts_with(&root.canonical))
        {
            return Ok(lexical);
        }
        Err(Error::invalid_params().data(format!(
            "path is outside the workspace boundary: {}",
            lexical.display()
        )))
    }

    fn assert_canonical_within(&self, path: &Path) -> Result<(), Error> {
        if self
            .roots
            .iter()
            .any(|root| path.starts_with(&root.canonical))
        {
            return Ok(());
        }
        Err(Error::invalid_params().data(format!(
            "path is outside the workspace boundary: {}",
            path.display()
        )))
    }

    async fn read_range(
        &self,
        path: &Path,
        start_line: u32,
        limit: Option<u32>,
    ) -> Result<String, Error> {
        if limit == Some(0) {
            return Ok(String::new());
        }
        let file = tokio::fs::File::open(path).await.map_err(fs_error)?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut output = Vec::new();
        let mut current_line = 1_u32;
        let mut selected = 0_u32;
        let mut scanned = 0_usize;

        loop {
            line.clear();
            let bytes = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(fs_error)?;
            if bytes == 0 {
                break;
            }
            scanned = scanned.saturating_add(bytes);
            if scanned > MAX_FILE_SCAN_BYTES {
                return Err(Error::invalid_request().data(format!(
                    "ACP file range scan exceeds {MAX_FILE_SCAN_BYTES} bytes"
                )));
            }
            if current_line >= start_line {
                if line.ends_with(b"\n") {
                    line.pop();
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                }
                if selected > 0 {
                    output.push(b'\n');
                }
                output.extend_from_slice(&line);
                selected = selected.saturating_add(1);
                if output.len() > MAX_FILE_CONTENT_BYTES {
                    return Err(Error::invalid_request().data(format!(
                        "ACP file read exceeds {MAX_FILE_CONTENT_BYTES} bytes"
                    )));
                }
                if limit.is_some_and(|limit| selected >= limit) {
                    break;
                }
            }
            current_line = current_line.saturating_add(1);
        }
        String::from_utf8(output)
            .map_err(|_| Error::invalid_request().data("ACP file is not valid UTF-8 text"))
    }
}

fn ensure_not_cancelled(cancellation: Option<&RequestCancellation>) -> Result<(), Error> {
    if cancellation.is_some_and(RequestCancellation::is_cancelled) {
        Err(Error::request_cancelled())
    } else {
        Ok(())
    }
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::invalid_params().data("workspace roots must be absolute"));
    }
    Ok(normalize(path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

fn fs_error(error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::resource_not_found(None).data(error.to_string())
    } else {
        Error::internal_error().data(error.to_string())
    }
}

fn ignored_context_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".cache"
            | ".next"
            | ".turbo"
            | "build"
            | "coverage"
            | "dist"
            | "node_modules"
            | "target"
            | "vendor"
    )
}

fn is_context_text_path(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase();
    if matches!(
        extension.as_str(),
        "c" | "cc"
            | "clj"
            | "cljs"
            | "cmake"
            | "cpp"
            | "cs"
            | "css"
            | "csv"
            | "dart"
            | "ex"
            | "exs"
            | "go"
            | "graphql"
            | "h"
            | "hpp"
            | "html"
            | "java"
            | "js"
            | "json"
            | "jsx"
            | "kt"
            | "kts"
            | "less"
            | "lua"
            | "md"
            | "mdx"
            | "mjs"
            | "php"
            | "proto"
            | "py"
            | "r"
            | "rb"
            | "rs"
            | "sass"
            | "scala"
            | "scss"
            | "sh"
            | "sql"
            | "svelte"
            | "swift"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "vue"
            | "xml"
            | "yaml"
            | "yml"
            | "zig"
    ) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();
    [
        ".env",
        ".gitignore",
        ".npmrc",
        "dockerfile",
        "gemfile",
        "justfile",
        "makefile",
        "readme",
    ]
    .iter()
    .any(|candidate| name == *candidate || name.starts_with(&format!("{candidate}.")))
}

fn context_match_score(path: &str, query: &str, terms: &[String]) -> Option<usize> {
    let path = path.to_lowercase();
    if !terms.iter().all(|term| path.contains(term)) {
        return None;
    }
    let name = path.rsplit('/').next().unwrap_or(path.as_str());
    Some(if query.is_empty() {
        path.matches('/').count() * 100 + path.len()
    } else if path == query || name == query {
        0
    } else if name.starts_with(query) {
        10 + name.len()
    } else if let Some(index) = name.find(query) {
        100 + index * 4 + name.len()
    } else if path.starts_with(query) {
        500 + path.len()
    } else if let Some(index) = path.find(query) {
        1_000 + index * 4 + path.len()
    } else {
        2_000 + path.len()
    })
}

fn context_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "json" => "application/json",
        "xml" => "application/xml",
        "html" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "jsx" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "yaml" | "yml" => "application/yaml",
        "md" | "mdx" => "text/markdown",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_error_data(error: Error) -> String {
        error
            .data
            .map(|value| value.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn reads_ranges_and_rejects_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("fixture.txt");
        std::fs::write(&file, "one\r\ntwo\nthree").unwrap();
        let fs = WorkspaceFileSystem::new(directory.path(), false, &[]).unwrap();
        let response = fs
            .read(ReadTextFileRequest::new("session", &file).line(2).limit(1))
            .await
            .unwrap();
        assert_eq!(response.content, "two");
        assert!(
            fs.read(ReadTextFileRequest::new(
                "session",
                directory.path().join("../outside")
            ))
            .await
            .is_err()
        );
        assert!(
            request_error_data(
                fs.read(ReadTextFileRequest::new("session", &file).line(0))
                    .await
                    .unwrap_err()
            )
            .contains("1-based")
        );
    }

    #[tokio::test]
    async fn writes_inside_the_workspace_and_bounds_content_before_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("bounded.txt");
        std::fs::write(&file, "stable").unwrap();
        let fs = WorkspaceFileSystem::new(directory.path(), false, &[]).unwrap();

        fs.write(WriteTextFileRequest::new("session", &file, "updated"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "updated");

        let oversized = "x".repeat(MAX_FILE_CONTENT_BYTES + 1);
        assert!(
            request_error_data(
                fs.write(WriteTextFileRequest::new("session", &file, oversized))
                    .await
                    .unwrap_err()
            )
            .contains("write exceeds")
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "updated");

        std::fs::write(
            &file,
            format!("head\n{}", "x".repeat(MAX_FILE_CONTENT_BYTES + 1)),
        )
        .unwrap();
        assert!(
            fs.read(ReadTextFileRequest::new("session", &file))
                .await
                .is_err()
        );
        let response = fs
            .read(ReadTextFileRequest::new("session", &file).line(1).limit(1))
            .await
            .unwrap();
        assert_eq!(response.content, "head");
    }

    #[tokio::test]
    async fn confines_primary_and_additional_roots_and_honors_read_only() {
        let primary = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let extra = additional.path().join("extra.txt");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&extra, "extra").unwrap();
        std::fs::write(&secret, "secret").unwrap();

        let fs =
            WorkspaceFileSystem::new(primary.path(), false, &[additional.path().to_path_buf()])
                .unwrap();
        assert_eq!(
            fs.read(ReadTextFileRequest::new("session", &extra))
                .await
                .unwrap()
                .content,
            "extra"
        );
        let new_file = additional.path().join("new.txt");
        fs.write(WriteTextFileRequest::new("session", &new_file, "new"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(new_file).unwrap(), "new");
        assert!(
            fs.read(ReadTextFileRequest::new("session", secret))
                .await
                .is_err()
        );
        assert!(
            fs.read(ReadTextFileRequest::new("session", "relative.txt"))
                .await
                .is_err()
        );

        let read_only = WorkspaceFileSystem::new(primary.path(), true, &[]).unwrap();
        assert!(
            request_error_data(
                read_only
                    .write(WriteTextFileRequest::new(
                        "session",
                        primary.path().join("blocked.txt"),
                        "x",
                    ))
                    .await
                    .unwrap_err()
            )
            .contains("read-only")
        );
    }

    #[tokio::test]
    async fn searches_bounded_context_and_embeds_selected_text() {
        let primary = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        std::fs::create_dir(primary.path().join("src")).unwrap();
        std::fs::create_dir(primary.path().join("node_modules")).unwrap();
        let source = primary.path().join("src/safe-context.ts");
        std::fs::write(&source, "export const safe = true;\n").unwrap();
        std::fs::write(primary.path().join("node_modules/hidden.ts"), "hidden").unwrap();
        std::fs::write(additional.path().join("extra-context.md"), "# Extra\n").unwrap();
        let fs =
            WorkspaceFileSystem::new(primary.path(), false, &[additional.path().to_path_buf()])
                .unwrap();

        let matches = fs.search_context("safe con").await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["name"], "safe-context.ts");
        assert_eq!(matches[0]["relativePath"], "src/safe-context.ts");
        assert!(fs.search_context("hidden").await.unwrap().is_empty());
        assert_eq!(fs.search_context("extra").await.unwrap().len(), 1);

        let attachment = fs.read_context(&source).await.unwrap();
        assert_eq!(attachment["name"], "src/safe-context.ts");
        assert_eq!(attachment["block"]["type"], "resource");
        assert_eq!(
            attachment["block"]["resource"]["mimeType"],
            "text/typescript"
        );
        assert_eq!(
            attachment["block"]["resource"]["text"],
            "export const safe = true;\n"
        );
    }

    #[tokio::test]
    async fn rejects_unsafe_binary_and_oversized_context() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let binary = directory.path().join("binary.png");
        let large = directory.path().join("large.txt");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&binary, [0, 1, 2]).unwrap();
        std::fs::write(&large, "x".repeat(MAX_CONTEXT_BYTES as usize + 1)).unwrap();
        std::fs::write(&secret, "secret").unwrap();
        let fs = WorkspaceFileSystem::new(directory.path(), false, &[]).unwrap();

        assert!(fs.read_context(&binary).await.is_err());
        assert!(fs.read_context(&large).await.is_err());
        assert!(fs.read_context(&secret).await.is_err());
        assert!(fs.search_context("large").await.unwrap().is_empty());
        assert!(
            fs.search_context(&"x".repeat(MAX_CONTEXT_QUERY_LENGTH + 1))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_that_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let link = directory.path().join("escape");
        symlink(outside.path(), &link).unwrap();
        let fs = WorkspaceFileSystem::new(directory.path(), false, &[]).unwrap();
        assert!(
            fs.read(ReadTextFileRequest::new("session", link))
                .await
                .is_err()
        );

        let outside_directory = tempfile::tempdir().unwrap();
        let directory_link = directory.path().join("outside-directory");
        symlink(outside_directory.path(), &directory_link).unwrap();
        assert!(
            fs.write(WriteTextFileRequest::new(
                "session",
                directory_link.join("escaped.txt"),
                "must not escape",
            ))
            .await
            .is_err()
        );
        assert!(!outside_directory.path().join("escaped.txt").exists());
    }
}
