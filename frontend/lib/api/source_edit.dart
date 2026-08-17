import 'api_client.dart';

/// Effective content of a single source file for an already-added package.
/// The pristine content is always present so the UI can fall back to it (and
/// offer a "revert" action) even if the stored patch no longer applies
/// cleanly to the current upstream source.
class SourceFileContent {
  SourceFileContent({
    required this.path,
    required this.originalContent,
    this.patchedContent,
    this.patchError,
  });

  final String path;
  final String originalContent;
  final String? patchedContent;
  final String? patchError;

  factory SourceFileContent.fromJson(Map<String, dynamic> json) {
    return SourceFileContent(
      path: json['path'] as String,
      originalContent: json['original_content'] as String,
      patchedContent: json['patched_content'] as String?,
      patchError: json['patch_error'] as String?,
    );
  }
}

/// Client for browsing/editing the source files of an already-added package.
extension SourceEditAPI on ApiClient {
  Future<List<String>> getSourceFiles(int id) async {
    final resp = await getRawClient().get("/package/$id/source/files");
    return (resp.data['files'] as List).cast<String>();
  }

  Future<SourceFileContent> getSourceFile({
    required int id,
    required String path,
  }) async {
    final resp = await getRawClient().get(
      "/package/$id/source/file",
      queryParameters: {'path': path},
    );
    return SourceFileContent.fromJson(resp.data);
  }

  Future<void> updateSourceFile({
    required int id,
    required String path,
    required String content,
  }) async {
    await getRawClient().put(
      "/package/$id/source/file",
      data: {'path': path, 'content': content},
    );
  }
}
