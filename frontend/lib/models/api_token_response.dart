import 'package:freezed_annotation/freezed_annotation.dart';

part 'api_token_response.g.dart';

@JsonSerializable()
class ApiTokenResponse {
  final String token;

  ApiTokenResponse({required this.token});

  factory ApiTokenResponse.fromJson(Map<String, dynamic> json) =>
      _$ApiTokenResponseFromJson(json);
  Map<String, dynamic> toJson() => _$ApiTokenResponseToJson(this);
}
