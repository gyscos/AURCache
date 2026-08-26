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
  final String upstream_version;

  SimplePackage({
    required this.id,
    required this.name,
    required this.status,
    required this.latest_version,
    required this.upstream_version,
    required this.outofdate,
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
  );

  static bool _fromJson(num value) => value != 0;
}
