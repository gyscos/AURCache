import 'package:aurcache/api/API.dart';
import 'package:aurcache/api/statistics.dart';
import 'package:aurcache/models/api_token_response.dart';
import 'package:aurcache/models/user_info.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';
import 'package:toastification/toastification.dart';

/// Builds the "API Token" settings tile as a plain `SettingsTile.navigation`
/// so it inherits the same title/description text styling as the other
/// entries in the settings list (e.g. "Builder Image"), instead of a
/// separately-styled widget.
SettingsTile apiTokenSettingsTile(
  BuildContext context,
  WidgetRef ref,
  UserInfo userInfo,
) {
  return SettingsTile.navigation(
    leading: const Icon(Icons.key),
    title: const Text('API Token'),
    description: Text(
      userInfo.hasApiToken
          ? 'Regenerate your personal API token for CLI or API access.'
          : 'Create a personal API token for CLI or API access.',
    ),
    trailing: const Icon(Icons.chevron_right),
    onPressed: (_) => showApiTokenDialog(context, ref, userInfo),
  );
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
          // Once a token has been generated in this dialog, treat it the
          // same as `hasApiToken` so the title/description/button reflect
          // "Regenerate" instead of sticking with the initial "Create".
          final hasToken = userInfo.hasApiToken || generatedToken != null;
          return AlertDialog(
            title: Text(hasToken ? 'Regenerate API Token' : 'Create API Token'),
            content: SizedBox(
              width: 480,
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    hasToken
                        ? 'Generating a new token will immediately replace the current one.'
                        : 'Create a personal token to authenticate API requests with a ******',
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
                      : (hasToken ? 'Regenerate' : 'Create'),
                ),
              ),
            ],
          );
        },
      );
    },
  );
}
