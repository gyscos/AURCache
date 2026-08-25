import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/settings.dart';

part 'settings.g.dart';

@riverpod
Future<ApplicationSettings> getSettings(Ref ref, {String? pkgbase}) async {
  final resp = await API.getRawClient().get(
    pkgbase == null
        ? "/settings"
        : "/package/${Uri.encodeComponent(pkgbase)}/settings",
  );

  return ApplicationSettings.fromJson(resp.data);
}
