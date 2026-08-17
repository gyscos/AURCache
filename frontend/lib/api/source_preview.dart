import 'api_client.dart';

/// Result of merging an edit into an in-progress (pre-add) patch.
class SourcePreviewPatchResult {
  SourcePreviewPatchResult({
    required this.patch,
    required this.parses,
    this.parseError,
  });

  final String? patch;
  final bool parses;
  final String? parseError;

  factory SourcePreviewPatchResult.fromJson(Map<String, dynamic> json) {
    return SourcePreviewPatchResult(
      patch: json['patch'] as String?,
      parses: json['parses'] as bool,
      parseError: json['parse_error'] as String?,
    );
  }
}

/// Client for the stateless pre-add "source preview" endpoints. These take
/// the raw `source` (and an optional in-progress `patch`) directly in the
/// request body, so a source's files can be browsed/edited before the
/// package is ever added (e.g. to fix a malformed PKGBUILD before it's
/// parsed for the first time).
extension SourcePreviewAPI on ApiClient {
  Future<List<String>> previewSourceFiles({
    required Map<String, dynamic> source,
  }) async {
    final resp = await getRawClient().post(
      "/package/source/preview/files",
      data: {'source': source},
    );
    return (resp.data['files'] as List).cast<String>();
  }

  Future<String> previewSourceFile({
    required Map<String, dynamic> source,
    String? patch,
    required String path,
  }) async {
    final resp = await getRawClient().post(
      "/package/source/preview/file",
      data: {
        'source': source,
        if (patch != null) 'patch': patch,
        'path': path,
      },
    );
    return resp.data['content'] as String;
  }

  Future<SourcePreviewPatchResult> updatePreviewSourceFile({
    required Map<String, dynamic> source,
    String? patch,
    required String path,
    required String content,
  }) async {
    final resp = await getRawClient().put(
      "/package/source/preview/file",
      data: {
        'source': source,
        if (patch != null) 'patch': patch,
        'path': path,
        'content': content,
      },
    );
    return SourcePreviewPatchResult.fromJson(resp.data);
  }
}
