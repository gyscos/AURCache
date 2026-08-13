import 'package:freezed_annotation/freezed_annotation.dart';
part 'user_info.g.dart';

@JsonSerializable()
class UserInfo {
  final String? username;
  @JsonKey(name: 'has_api_token')
  final bool hasApiToken;

  UserInfo({required this.username, required this.hasApiToken});

  factory UserInfo.fromJson(Map<String, dynamic> json) =>
      _$UserInfoFromJson(json);
  Map<String, dynamic> toJson() => _$UserInfoToJson(this);
}
