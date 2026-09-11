# yt-dlp plugin: revive the Android-VR client and add optional OAuth bearer
# auth for it (the "VR OAuth" full-quality route in s2u-yt, v2026-09-10).
#
# Two effects, both applied at import time (yt-dlp imports every module under
# the plugin yt_dlp_plugins/extractor/ package at startup):
#
#  1. INNERTUBE_CLIENTS['android_vr'] is bumped to the community-verified
#     working fingerprint (plugin.video.youtube PR #1482, merged 2026-08-19).
#     yt-dlp 2026.08.19 ships clientVersion 1.65.10 (Quest 3/eureka), which
#     has answered 403 for ALL formats since 2026-08-17 (yt-dlp PR #17461).
#
#  2. When $S2U_VR_ACCESS_TOKEN is set (the s2u-yt wrapper exports it only for
#     its android_vr phase), `Authorization: Bearer <token>` is added to the
#     InnerTube API request headers while the active client is android_vr.
#     Media (googlevideo) requests never pass through generate_api_headers,
#     so the bearer never leaks onto stream GETs — they stay header-clean and
#     playable by mpv/MPD with plain range requests.
from __future__ import annotations

import os

from yt_dlp.extractor.youtube._base import INNERTUBE_CLIENTS, YoutubeBaseInfoExtractor

_ENV_TOKEN = 'S2U_VR_ACCESS_TOKEN'

# Android-VR fingerprint that still passes YouTube's client checks
# (Pico A8110, Python 27 -> VR app 1.73.21; see plugin.video.youtube #1482).
_VR_CLIENT_FINGERPRINT = {
    'clientVersion': '1.73.21',
    'deviceMake': 'Pico',
    'deviceModel': 'A8110',
    'androidSdkVersion': 29,
    'userAgent': 'com.google.android.apps.youtube.vr.pico/1.73.21 (Linux; U; Android 10; A8110-user Build/5.13.7) gzip',
    'osName': 'Android',
    'osVersion': '10',
    'osBuild': '5.13.7',
}

INNERTUBE_CLIENTS.setdefault('android_vr', {})
INNERTUBE_CLIENTS['android_vr'].setdefault('INNERTUBE_CONTEXT', {})
INNERTUBE_CLIENTS['android_vr']['INNERTUBE_CONTEXT'].setdefault('client', {})
INNERTUBE_CLIENTS['android_vr']['INNERTUBE_CONTEXT']['client'].update(_VR_CLIENT_FINGERPRINT)

_orig_generate_api_headers = YoutubeBaseInfoExtractor.generate_api_headers


def _generate_api_headers_with_vr_oauth(self, *, default_client='web', **kwargs):
    headers = _orig_generate_api_headers(self, default_client=default_client, **kwargs)
    token = os.environ.get(_ENV_TOKEN)
    if token and default_client == 'android_vr':
        headers['Authorization'] = f'Bearer {token}'
    return headers


YoutubeBaseInfoExtractor.generate_api_headers = _generate_api_headers_with_vr_oauth
