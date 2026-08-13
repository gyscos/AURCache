// This is a basic Flutter widget test.
//
// To perform an interaction with a widget in your test, use the WidgetTester
// utility in the flutter_test package. For example, you can send tap and scroll
// gestures. You can also use WidgetTester to find child widgets in the widget
// tree, read text, and verify that the values of widget properties are correct.

import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_settings_ui/flutter_settings_ui.dart';

import 'package:aurcache/components/api_token_settings.dart';
import 'package:aurcache/models/user_info.dart';
import 'package:flutter/material.dart';

void main() {
  testWidgets(
    'settings token tile shows a regenerate action for authenticated users',
    (WidgetTester tester) async {
      await tester.pumpWidget(
        ProviderScope(
          child: MaterialApp(
            home: Scaffold(
              body: Consumer(
                builder: (context, ref, _) => SettingsList(
                  sections: [
                    SettingsSection(
                      title: const Text('API Access'),
                      tiles: [
                        apiTokenSettingsTile(
                          context,
                          ref,
                          UserInfo(username: 'alice', hasApiToken: true),
                        ),
                      ],
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      );
      await tester.pump();

      expect(find.text('API Token'), findsOneWidget);
      expect(
        find.text('Regenerate your personal API token for CLI or API access.'),
        findsOneWidget,
      );
    },
  );
}
