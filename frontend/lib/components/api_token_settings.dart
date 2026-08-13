import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/statistics.dart';
import 'package:aurcache/models/api_token_response.dart';
import 'package:aurcache/models/user_info.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:toastification/toastification.dart';

class ApiTokenSettingsContent extends ConsumerWidget {
  const ApiTokenSettingsContent({super.key, required this.userInfo});

  final UserInfo userInfo;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(20, 14, 20, 14),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('API Token', style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 4),
          Text(
            userInfo.hasApiToken
                ? 'Regenerate your personal API token for CLI or API access.'
                : 'Create a personal API token for CLI or API access.',
            style: Theme.of(context).textTheme.bodySmall?.copyWith(
              color: Theme.of(
                context,
              ).textTheme.bodySmall?.color?.withValues(alpha: 0.75),
            ),
          ),
          const SizedBox(height: 12),
          OutlinedButton.icon(
            onPressed: () => showApiTokenDialog(context, ref, userInfo),
            icon: const Icon(Icons.key),
            label: Text(
              userInfo.hasApiToken ? 'Regenerate Token' : 'Create Token',
            ),
          ),
        ],
      ),
    );
  }
}

Future<void> showApiTokenDialog(
  BuildContext context,
  WidgetRef ref,
  UserInfo userInfo,
) async {
  bool isLoading = false;
  String? generatedToken;

  await showDialog<void>(
    context: context,
    builder: (dialogContext) {
      return StatefulBuilder(
        builder: (context, setState) {
          return AlertDialog(
            title: Text(
              userInfo.hasApiToken
                  ? 'Regenerate API Token'
                  : 'Create API Token',
            ),
            content: SizedBox(
              width: 480,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    userInfo.hasApiToken
                        ? 'Generating a new token will immediately replace the current one.'
                        : 'Create a personal token to authenticate API requests with a Bearer header.',
                  ),
                  const SizedBox(height: 16),
                  if (generatedToken != null) ...[
                    SelectableText(
                      generatedToken!,
                      style: Theme.of(
                        context,
                      ).textTheme.bodyMedium?.copyWith(fontFamily: 'monospace'),
                    ),
                    const SizedBox(height: 12),
                    Text(
                      'Copy this token now. It is only shown once.',
                      style: Theme.of(context).textTheme.bodySmall,
                    ),
                  ],
                ],
              ),
            ),
            actions: [
              if (generatedToken != null)
                TextButton.icon(
                  onPressed: () async {
                    await Clipboard.setData(
                      ClipboardData(text: generatedToken!),
                    );
                    if (context.mounted) {
                      toastification.show(
                        context: context,
                        title: const Text('Token copied to clipboard'),
                        autoCloseDuration: const Duration(seconds: 3),
                        type: ToastificationType.success,
                      );
                    }
                  },
                  icon: const Icon(Icons.copy),
                  label: const Text('Copy'),
                ),
              TextButton(
                onPressed: isLoading
                    ? null
                    : () => Navigator.of(dialogContext).pop(),
                child: Text(generatedToken == null ? 'Cancel' : 'Close'),
              ),
              FilledButton(
                onPressed: isLoading
                    ? null
                    : () async {
                        setState(() {
                          isLoading = true;
                        });
                        try {
                          final ApiTokenResponse response = await API
                              .regenerateApiToken();
                          setState(() {
                            generatedToken = response.token;
                          });
                          ref.invalidate(userInfoProvider);
                        } catch (error) {
                          if (context.mounted) {
                            toastification.show(
                              context: context,
                              title: Text('Failed to generate token: $error'),
                              autoCloseDuration: const Duration(seconds: 5),
                              type: ToastificationType.error,
                            );
                          }
                        } finally {
                          setState(() {
                            isLoading = false;
                          });
                        }
                      },
                child: Text(
                  isLoading
                      ? 'Working...'
                      : userInfo.hasApiToken
                      ? 'Regenerate'
                      : 'Create',
                ),
              ),
            ],
          );
        },
      );
    },
  );
}
