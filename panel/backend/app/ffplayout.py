"""Client for ffplayout's REST API and its server-sent playout events."""

import asyncio
import base64
import json
import logging
import time
from typing import Any, AsyncIterator

import httpx

log = logging.getLogger("panel.ffplayout")

# Access tokens live 45 minutes; renew them well before that.
RENEW_BEFORE_SECONDS = 10 * 60


class FFplayoutError(Exception):
    pass


def _token_expiry(token: str) -> float:
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        return float(json.loads(base64.urlsafe_b64decode(payload))["exp"])
    except (IndexError, KeyError, ValueError):
        return time.time() + 30 * 60


class FFplayout:
    def __init__(self, base_url: str, user: str, password: str, channel: int):
        self.base_url = base_url
        self.user = user
        self.password = password
        self.channel = channel
        self.http = httpx.AsyncClient(base_url=base_url, timeout=15.0)
        self._access: str | None = None
        self._refresh: str | None = None
        self._expires_at = 0.0
        self._auth_lock = asyncio.Lock()

    async def close(self) -> None:
        await self.http.aclose()

    # --- auth ----------------------------------------------------------------

    async def login(self) -> None:
        resp = await self.http.post(
            "/auth/login", json={"username": self.user, "password": self.password}
        )
        if resp.status_code != 200:
            raise FFplayoutError(f"ffplayout login failed ({resp.status_code}): {resp.text}")
        self._store_tokens(resp.json())

    async def _renew(self) -> None:
        if self._refresh:
            resp = await self.http.post("/auth/refresh", json={"refresh": self._refresh})
            if resp.status_code == 200:
                self._store_tokens(resp.json())
                return
            log.info("Token refresh rejected (%s), logging in again", resp.status_code)
        await self.login()

    def _store_tokens(self, data: dict) -> None:
        if "access" not in data:
            raise FFplayoutError(f"Unexpected login response: {data}")
        self._access = data["access"]
        self._refresh = data.get("refresh")
        self._expires_at = _token_expiry(self._access)

    async def _token(self, force: bool = False) -> str:
        async with self._auth_lock:
            if force or not self._access or self._expires_at - time.time() < RENEW_BEFORE_SECONDS:
                await self._renew()
            assert self._access
            return self._access

    async def request(self, method: str, path: str, ok: tuple[int, ...] = (200,), **kwargs) -> Any:
        for attempt in range(2):
            token = await self._token(force=attempt > 0)
            resp = await self.http.request(
                method, path, headers={"Authorization": f"Bearer {token}"}, **kwargs
            )
            if resp.status_code != 401:
                break
        if resp.status_code not in ok:
            raise FFplayoutError(f"{method} {path} → {resp.status_code}: {resp.text[:300]}")
        if not resp.content:
            return None
        try:
            return resp.json()
        except ValueError:
            return resp.text

    # --- setup and configuration ------------------------------------------------

    async def setup_required(self) -> bool:
        resp = await self.http.get("/api/setup")
        resp.raise_for_status()
        return bool(resp.json().get("required"))

    async def complete_setup(self, mail: str, storage: str) -> None:
        resp = await self.http.post(
            "/api/setup",
            json={
                "username": self.user,
                "mail": mail,
                "password": self.password,
                "two_factor": False,
                "logs": "/config/logs",
                "playlists": "/playlists",
                "public": "/config/public",
                "storage": storage,
                "shared": False,
                "smtp_server": "",
                "smtp_user": "",
                "smtp_password": "",
                "smtp_starttls": False,
                "smtp_port": 465,
            },
        )
        if resp.status_code != 200:
            raise FFplayoutError(f"ffplayout setup failed ({resp.status_code}): {resp.text}")

    async def get_config(self) -> dict:
        return await self.request("GET", f"/api/playout/config/{self.channel}")

    async def put_config(self, config: dict) -> dict:
        return await self.request("PUT", f"/api/playout/config/{self.channel}", json=config)

    async def outputs(self) -> list[dict]:
        """One record per output mode (hls, stream, desktop); config.output.id picks one."""
        return await self.request("GET", f"/api/playout/outputs/{self.channel}")

    async def get_channel(self) -> dict:
        return await self.request("GET", f"/api/channel/{self.channel}")

    async def patch_channel(self, channel: dict) -> None:
        await self.request("PATCH", f"/api/channel/{self.channel}", json=channel)

    # --- playout ---------------------------------------------------------------

    async def process(self, command: str) -> str:
        return await self.request(
            "POST", f"/api/control/{self.channel}/process", json={"command": command}
        )

    async def playout(self, control: str) -> dict:
        return await self.request(
            "POST", f"/api/control/{self.channel}/playout", json={"control": control}
        )

    async def send_text(self, preset: dict) -> None:
        await self.request("POST", f"/api/control/{self.channel}/text", json=preset)

    async def save_playlist(self, date: str, program: list[dict]) -> None:
        # 409 means the stored playlist is already identical; nothing to do.
        await self.request(
            "POST",
            f"/api/playlist/{self.channel}",
            ok=(200, 409),
            json={"channel": "Channel 1", "date": date, "program": program},
        )

    async def delete_playlist(self, date: str) -> None:
        await self.request(
            "DELETE", f"/api/playlist/{self.channel}/{date}", ok=(200, 204, 400, 404)
        )

    async def events(self) -> AsyncIterator[dict | str]:
        """Playout status, pushed by ffplayout four times a second."""
        uuid = (await self.request("POST", "/api/generate-uuid"))["uuid"]
        url = f"/data/event/{self.channel}"
        params = {"endpoint": "playout", "uuid": uuid}
        timeout = httpx.Timeout(15.0, read=30.0)
        async with self.http.stream("GET", url, params=params, timeout=timeout) as resp:
            if resp.status_code != 200:
                raise FFplayoutError(f"Event stream refused: {resp.status_code}")
            async for line in resp.aiter_lines():
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if data.startswith("{"):
                    yield json.loads(data)
                else:
                    yield data
