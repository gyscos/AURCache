import 'package:aurcache/components/add_package_popup.dart';
import 'package:aurcache/providers/statistics.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../constants/color_constants.dart';
import '../../models/user_info.dart';
import '../../utils/responsive.dart';

class Header extends ConsumerWidget {
  const Header({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final userInfo = ref.watch(userInfoProvider);

    return Row(
      children: [
        if (context.mobile)
          IconButton(
            icon: const Icon(Icons.menu),
            onPressed: () {
              Scaffold.of(context).openDrawer();
            },
          ),
        if (context.desktop)
          Column(
            mainAxisAlignment: MainAxisAlignment.start,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              userInfo.when(
                loading: () => Text(
                  "Hi, Arch User :)",
                  style: Theme.of(context).textTheme.titleLarge,
                ),
                error: (_, __) => Text(
                  "Hi, Arch User :)",
                  style: Theme.of(context).textTheme.titleLarge,
                ),
                data: (UserInfo data) => Text(
                  data.username == null
                      ? "Hi, Arch User :)"
                      : "Hi, ${data.username} :)",
                  style: Theme.of(context).textTheme.titleLarge,
                ),
              ),
              const SizedBox(height: 8),
              Text(
                "Welcome to your personal build server",
                style: Theme.of(context).textTheme.titleSmall,
              ),
            ],
          ),
        Spacer(flex: context.desktop ? 2 : 1),
        OutlinedButton.icon(
          style: OutlinedButton.styleFrom(
            backgroundColor: const Color(0xff0059FF),
            side: const BorderSide(color: Color(0xff0059FF), width: 0),
            shape: RoundedRectangleBorder(
              borderRadius: BorderRadius.circular(8),
            ),
            padding: const EdgeInsets.symmetric(
              horizontal: defaultPadding,
              vertical: defaultPadding,
            ),
          ),
          onPressed: () {
            showPackageAddPopupNew(context);

            //context.push("/aur");
          },
          icon: const Icon(Icons.add, color: Colors.white),
          label: const Text(
            "Add Package",
            style: TextStyle(color: Colors.white),
          ),
        ),
        //ProfileCard()
      ],
    );
  }
}
