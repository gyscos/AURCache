import '../models/settings.dart';
import 'api_client.dart';

/// Base path for settings in a given scope.
///
/// Per-package settings are a sub-resource of the package rather than a
/// `?pkgbase=` filter: a pkgbase may contain `+` (187 AUR packages do, such as
/// `aewm++`), which is literal in a path segment but decodes to a space in a
/// query value.
String _settingsBase(String? pkgbase) => pkgbase == null
    ? '/settings'
    : '/package/${Uri.encodeComponent(pkgbase)}/settings';

extension SettingsAPI on ApiClient {
  /// Update a single setting by key. The backend stores everything as text;
  /// callers must convert numbers/cron strings/etc. to a string first.
  Future<bool> patchSetting(String key, String value, {String? pkgbase}) async {
    final resp = await getRawClient().patch(
      "${_settingsBase(pkgbase)}/$key",
      data: {"value": value},
    );
    return resp.statusCode == 200;
  }

  /// Reset a single setting back to its default by deleting any stored
  /// override.
  Future<bool> resetSetting(String key, {String? pkgbase}) async {
    final resp = await getRawClient().delete("${_settingsBase(pkgbase)}/$key");
    return resp.statusCode == 200;
  }

  /// Fetch a single setting (used for large blobs like makepkg.conf /
  /// pacman.conf that are not part of the bulk dashboard payload).
  Future<SingleSetting> getSetting(String key, {String? pkgbase}) async {
    final resp = await getRawClient().get("${_settingsBase(pkgbase)}/$key");
    return SingleSetting.fromJson(resp.data as Map<String, dynamic>);
  }
}
