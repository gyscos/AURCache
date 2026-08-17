import 'api_client.dart';

/// Client for the stateless pre-add "source preview" endpoints. These take
/// the raw `source` directly in the request body, so a source's (pristine)
/// files can be browsed before the package is ever added (e.g. to inspect/
/// fix a malformed PKGBUILD before it's parsed for the first time). Edits
/// are kept purely client-side (as full file contents) until the package is
/// actually added, at which point they're sent as `AddPackage.patched_files`
/// and diffed server-side.
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
    required String path,
  }) async {
    final resp = await getRawClient().post(
      "/package/source/preview/file",
      data: {'source': source, 'path': path},
    );
    return resp.data['original_content'] as String;
  }
}
