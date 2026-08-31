import 'package:freezed_annotation/freezed_annotation.dart';
part 'simple_packge.g.dart';

@JsonSerializable()
class SimplePackage {
  final int id;
  final String name;
  @JsonKey(fromJson: _fromJson)
  final bool outofdate;
  final int status;

  /// Null when the package has never produced a build, or when the only build
  /// so far has not worked out a version yet. A non-nullable String here made
  /// the whole list fail to deserialise on the null the server sends.
  final String? latest_version;

  /// Null until a version check has determined it; see the note above.
  final String? upstream_version;

  /// Combined size in bytes of this package's artifacts in the repository.
  ///
  /// Null when there is nothing to total: the package has never built, or one
  /// of its artifacts has no recorded size. Never a partial sum.
  final int? total_size;

  SimplePackage({
    required this.id,
    required this.name,
    required this.status,
    required this.latest_version,
    required this.upstream_version,
    required this.outofdate,
    this.total_size,
  });

  factory SimplePackage.fromJson(Map<String, dynamic> json) =>
      _$SimplePackageFromJson(json);
  Map<String, dynamic> toJson() => _$SimplePackageToJson(this);

  factory SimplePackage.dummy() => SimplePackage(
    id: 42,
    name: 'MyPackage',
    status: 0,
    latest_version: '1.0.0',
    upstream_version: '1.0.0',
    outofdate: false,
    total_size: 1258291,
  );

  static bool _fromJson(num value) => value != 0;
}
