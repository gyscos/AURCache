import 'package:freezed_annotation/freezed_annotation.dart';

part 'extended_package.freezed.dart';
part 'extended_package.g.dart';

bool _fromJson(num value) => value != 0;

String _toString(PackageSource src) {
  return src.toString();
}

@freezed
sealed class ExtendedPackage with _$ExtendedPackage {
  factory ExtendedPackage({
    required int id,
    required String name,
    required bool directly_requested,
    required int status,
    // ignore: invalid_annotation_target
    @JsonKey(fromJson: _fromJson) required bool outofdate,
    required String? upstream_version,
    final String? latest_version,
    required List<String> selected_platforms,
    required List<String> selected_build_flags,
    required List<PackageDependency> dependencies,
    required List<PackageDependency> dependents,
    final List<String>? split_packages,
    // The artifacts actually in the repository, one per split package per
    // platform. Null on a response from a server that predates the field;
    // empty until the package has built successfully once.
    final List<PackageFile>? files,
    required bool has_patch,
    // Read from the package's source checkout rather than the AUR, so these
    // describe a git-sourced package too.
    String? description,
    String? project_url,
    String? licenses,
    String? maintainer,
    int? first_submitted,
    int? last_modified,
    // ignore: invalid_annotation_target
    @JsonKey(toJson: _toString) required PackageSource package_source,
  }) = _ExtendedPackage;

  factory ExtendedPackage.fromJson(Map<String, dynamic> json) =>
      _$ExtendedPackageFromJson(json);

  factory ExtendedPackage.dummy() => ExtendedPackage(
    id: 42,
    name: "Dummy",
    directly_requested: true,
    status: 0,
    outofdate: true,
    upstream_version: "1.0.0",
    selected_platforms: ["arm64"],
    selected_build_flags: ["--noconfirm", "--noprogressbar"],
    dependencies: [
      PackageDependency(id: 7, name: "dummy-lib", version_constraint: ">=1.0"),
    ],
    dependents: [
      PackageDependency(
        id: 8,
        name: "dummy-parent",
        version_constraint: ">=1.0",
      ),
    ],
    split_packages: null,
    files: [],
    has_patch: false,
    package_source: PackageSource.git(
      GitPackage(
        ref: "master",
        url: "http://dummyur.org",
        subfolder: "dummyfolder",
      ),
    ),
  );
}

/// One built artifact in the repository.
@freezed
sealed class PackageFile with _$PackageFile {
  const factory PackageFile({
    required String filename,
    required String platform,
    // Compressed download size in bytes. Null when it is not known, which is
    // not the same as a zero-byte file — the UI shows a dash rather than 0 B.
    required int? size,
  }) = _PackageFile;

  factory PackageFile.fromJson(Map<String, dynamic> json) =>
      _$PackageFileFromJson(json);
}

@freezed
sealed class PackageDependency with _$PackageDependency {
  const factory PackageDependency({
    required int id,
    required String name,
    required String version_constraint,
  }) = _PackageDependency;

  factory PackageDependency.fromJson(Map<String, dynamic> json) =>
      _$PackageDependencyFromJson(json);
}

@Freezed(unionKey: 'package_type', unionValueCase: FreezedUnionCase.pascal)
sealed class PackageSource with _$PackageSource {
  const factory PackageSource.aur(AurPackage aur) = Aur;
  const factory PackageSource.aurNotFound(AurNotFoundPackage aurNotFound) =
      AurNotFound;
  const factory PackageSource.git(GitPackage git) = Git;
  const factory PackageSource.upload(UploadPackage upload) = Upload;

  factory PackageSource.fromJson(Map<String, dynamic> json) {
    final type = json['package_type'];
    switch (type) {
      case 'Aur':
        return PackageSource.aur(AurPackage.fromJson(json));
      case 'AurNotFound':
        return PackageSource.aurNotFound(AurNotFoundPackage.fromJson(json));
      case 'Git':
        return PackageSource.git(GitPackage.fromJson(json));
      case 'Upload':
        return PackageSource.upload(UploadPackage.fromJson(json));
      default:
        throw UnsupportedError('Unknown package_type: $type');
    }
  }
}

@freezed
sealed class AurPackage with _$AurPackage {
  const factory AurPackage({
    required String name,
    required bool aur_flagged_outdated,
    required String aur_url,
  }) = _AurPackage;

  factory AurPackage.fromJson(Map<String, dynamic> json) =>
      _$AurPackageFromJson(json);
}

@freezed
sealed class GitPackage with _$GitPackage {
  const factory GitPackage({
    required String url,
    required String ref,
    required String subfolder,
  }) = _GitPackage;

  factory GitPackage.fromJson(Map<String, dynamic> json) =>
      _$GitPackageFromJson(json);
}

@freezed
sealed class UploadPackage with _$UploadPackage {
  const factory UploadPackage() = _UploadPackage;

  factory UploadPackage.fromJson(Map<String, dynamic> json) =>
      _$UploadPackageFromJson(json);
}

@freezed
sealed class AurNotFoundPackage with _$AurNotFoundPackage {
  const factory AurNotFoundPackage() = _AurNotFoundPackage;

  factory AurNotFoundPackage.fromJson(Map<String, dynamic> json) =>
      _$AurNotFoundPackageFromJson(json);
}
