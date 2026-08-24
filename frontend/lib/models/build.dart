import 'package:freezed_annotation/freezed_annotation.dart';
part 'build.g.dart';

@JsonSerializable()
class Build {
  final int id;
  final String pkg_name, platform;
  final int pkg_id;
  final String version;
  final int status;
  @JsonKey(fromJson: _fromJson)
  final DateTime? end_time;
  @JsonKey(fromJson: _fromJson)
  final DateTime start_time;

  /// Why this build is stuck, when it is enqueued and *no* approved worker can
  /// currently take it. Absent for a build merely queued behind a busy worker,
  /// so its presence always means something needs attention. Kept as a raw map
  /// rather than a generated model because it is a small tagged union that only
  /// ever gets rendered as one line of text.
  final Map<String, dynamic>? waiting_reason;

  Build({
    required this.id,
    required this.pkg_id,
    required this.pkg_name,
    required this.platform,
    required this.version,
    required this.start_time,
    required this.end_time,
    required this.status,
    this.waiting_reason,
  });

  factory Build.fromJson(Map<String, dynamic> json) => _$BuildFromJson(json);
  Map<String, dynamic> toJson() => _$BuildToJson(this);

  factory Build.dummy() => Build(
    id: 42,
    pkg_id: 0,
    pkg_name: "MyPackage",
    platform: "x86_64",
    version: "1.0.1",
    start_time: DateTime.now(),
    end_time: DateTime.now(),
    status: 0,
  );

  static dynamic _fromJson(int? value) =>
      value != null ? DateTime.fromMillisecondsSinceEpoch(value * 1000) : null;

  Duration get duration => (end_time ?? DateTime.now()).difference(start_time);

  /// [waiting_reason] rendered for display, or `null` when nothing is wrong.
  String? get waitingMessage {
    final reason = waiting_reason;
    if (reason == null) return null;
    switch (reason["kind"]) {
      case "affinity":
        final workers =
            (reason["workers"] as List?)?.cast<String>() ?? const <String>[];
        final who = workers.isEmpty ? "an offline worker" : workers.join(", ");
        return "Reserved for $who, which is offline";
      case "arch":
        return "No worker builds ${reason["arch"]}";
      case "offline":
        return "All capable workers are offline";
      default:
        return null;
    }
  }
}
