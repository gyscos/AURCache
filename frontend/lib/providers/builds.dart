import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:riverpod_annotation/riverpod_annotation.dart';

import '../api/API.dart';
import '../models/build.dart';

part 'builds.g.dart';

@riverpod
Future<List<Build>> listBuilds(Ref ref, {String? pkgbase, int? limit}) async {
  // Builds for a package are a sub-resource, not a query filter: `+` is
  // literal in a path segment but decodes to a space in a query value.
  String uri = pkgbase == null
      ? "/builds?"
      : "/package/${Uri.encodeComponent(pkgbase)}/builds?";

  if (limit != null) {
    uri += "limit=$limit";
  }

  final resp = await API.getRawClient().get(uri);

  final responseObject = resp.data as List;
  final List<Build> packages = responseObject
      .map((e) => Build.fromJson(e))
      .toList(growable: false);
  return packages;
}

@riverpod
Future<Build> getBuild(Ref ref, int id) async {
  final resp = await API.getRawClient().get("/build/$id");
  return Build.fromJson(resp.data);
}
