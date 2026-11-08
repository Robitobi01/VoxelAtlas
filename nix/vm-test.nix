{ pkgs, package }:

let
  fixtures = pkgs.writeText "voxelatlas-vm-fixtures.py" ''
    from pathlib import Path
    import struct
    import zlib

    root = Path("/srv/fixtures")
    for directory in ("world/region", "world/playerdata", "world/stats", "textures/blocks"):
        (root / directory).mkdir(parents=True, exist_ok=True)

    # Minecraft 1.12.2 NBT, with one stone block in one section in chunk (0, 0).
    def tag(kind, name, payload):
        name = name.encode()
        return bytes([kind]) + struct.pack(">H", len(name)) + name + payload

    def array(name, data):
        return tag(7, name, struct.pack(">i", len(data)) + data)

    blocks = bytearray(4096)
    blocks[8 * 16 + 8] = 1
    section = (
        tag(1, "Y", b"\x00")
        + array("Blocks", blocks)
        + array("Data", bytes(2048))
        + array("BlockLight", bytes(2048))
        + array("SkyLight", b"\xff" * 2048)
        + b"\x00"
    )
    level = (
        tag(3, "xPos", struct.pack(">i", 0))
        + tag(3, "zPos", struct.pack(">i", 0))
        + tag(4, "LastUpdate", struct.pack(">q", 1))
        + array("Biomes", bytes([1]) * 256)
        + tag(9, "Sections", b"\x0a" + struct.pack(">i", 1) + section)
        + b"\x00"
    )
    nbt = tag(10, "", tag(3, "DataVersion", struct.pack(">i", 1343)) + tag(10, "Level", level) + b"\x00")
    payload = zlib.compress(nbt)
    record = struct.pack(">I", len(payload) + 1) + b"\x02" + payload
    sectors = (len(record) + 4095) // 4096
    header = bytearray(8192)
    header[:4] = bytes([0, 0, 2, sectors])
    header[4096:4100] = struct.pack(">I", 1)
    (root / "world/region/r.0.0.mca").write_bytes(header + record.ljust(sectors * 4096, b"\x00"))

    # PNG encoding uses only the standard library, not external Minecraft assets.
    def png_chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    pixels = b"".join(
        b"\x00" + b"".join(bytes((180, 150, 100, 255) if (x + y) % 2 else (140, 120, 90, 255)) for x in range(16))
        for y in range(16)
    )
    png = (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 6, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(pixels))
        + png_chunk(b"IEND", b"")
    )
    (root / "textures/blocks/stone.png").write_bytes(png)
  '';
  verify = pkgs.writeText "voxelatlas-vm-verify.py" ''
    import json
    from pathlib import Path
    from urllib.error import HTTPError
    from urllib.request import urlopen

    output = Path("/var/lib/voxelatlas/output")

    def get(path, cache):
        with urlopen("http://localhost/map/" + path) as response:
            assert response.status == 200
            assert response.headers["Cache-Control"] == cache, dict(response.headers)
            return response.read()

    assert b"<html" in get("", "no-cache").lower()
    manifest = json.loads((output / "current.json").read_text())
    assert json.loads(get("current.json", "no-cache, must-revalidate")) == manifest
    assert manifest["version"] == 2
    assert manifest["tile_size"] == 256
    assert len(manifest["maps"]) == 1
    map_data = manifest["maps"][0]
    assert map_data["id"] == "tiny" and map_data["world"] == "fixture"
    assert "tiles" not in map_data
    assert map_data["tile_indexes"], "real chunk must produce tile indexes"
    immutable = "public, max-age=31536000, immutable"
    for index_hash in map_data["tile_indexes"].values():
        index = json.loads(get("indexes/" + index_hash + ".json", immutable))
        assert index["tiles"], "index must contain rendered tiles"
        for tile_hash in index["tiles"].values():
            tile = get("tiles/" + tile_hash + ".webp", immutable)
            assert tile[:4] == b"RIFF" and tile[8:12] == b"WEBP"

    players = json.loads(get("live/players.json", "public, max-age=5, must-revalidate"))
    assert players["players"] == []
    assert players["updated_at"] > 0
    json.loads(get("live/chunks.json", "public, max-age=5, must-revalidate"))
    assert (output / ".state/index.json").is_file()
    assert (output / "generations" / (manifest["generation"] + ".json")).is_file()
    for path in (".state/index.json", "generations/", "generations/" + manifest["generation"] + ".json"):
        try:
            urlopen("http://localhost/map/" + path)
        except HTTPError as error:
            assert error.code == 404, (path, error.code)
        else:
            raise AssertionError("private path was exposed: " + path)
  '';
in
pkgs.testers.runNixOSTest {
  name = "voxelatlas-service";

  nodes.machine = { ... }: {
    imports = [ ./module.nix ];
    virtualisation.memorySize = 1024;
    virtualisation.cores = 2;
    environment.systemPackages = [ pkgs.python3 ];
    services.voxelatlas = {
      enable = true;
      inherit package;
      settings = {
        textures = "/srv/fixtures/textures";
        workers = 2;
        cache_mib = 16;
        tile_size = 256;
        worlds.fixture.path = "/srv/fixtures/world";
        maps = [ {
          id = "tiny";
          world = "fixture";
          view = "topdown";
          mode = "plain";
          default_zoom = 0;
        } ];
      };
      memoryMax = "512M";
      memoryHigh = "384M";
      cpuQuota = "200%";
      # Avoid calendar-boundary races; exercise the enabled jobs explicitly below.
      render.schedule = "2099-01-01 00:00:00";
      dynamic = {
        enable = true;
        playerDataDirectory = "/srv/fixtures/world/playerdata";
        schedule = "2099-01-01 00:00:00";
      };
      nginx.enable = true;
    };
    system.stateVersion = "25.11";
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("nginx.service")
    machine.wait_for_open_port(80)
    machine.succeed("python3 ${fixtures}")

    with subtest("enabled calendar timers and constrained jobs"):
        for job in ("voxelatlas-render", "voxelatlas-update-data"):
            machine.succeed(f"systemctl is-enabled {job}.timer")
            machine.wait_for_unit(f"{job}.timer")
            calendar = machine.succeed(f"systemctl show {job}.timer -p TimersCalendar --value")
            assert "2099-01-01" in calendar, calendar
        for unit in ("voxelatlas-render.service", "voxelatlas-update-data.service", "voxelatlas.slice"):
            properties = machine.succeed(f"systemctl show {unit} -p MemoryMax -p MemoryHigh -p MemorySwapMax -p CPUQuotaPerSecUSec -p Slice")
            properties = dict(line.split("=", 1) for line in properties.splitlines())
            assert properties["MemoryMax"] == "536870912", properties
            assert properties["MemoryHigh"] == "402653184", properties
            assert properties["MemorySwapMax"] == "0", properties
            assert properties["CPUQuotaPerSecUSec"] == "2s", properties
            if unit.endswith(".service"):
                assert properties["Slice"] == "voxelatlas.slice", properties
        machine.succeed("systemctl start voxelatlas.slice")
        machine.succeed("test $(cat /sys/fs/cgroup/voxelatlas.slice/memory.max) = 536870912")
        machine.succeed("test $(cat /sys/fs/cgroup/voxelatlas.slice/memory.high) = 402653184")
        assert machine.succeed("cat /sys/fs/cgroup/voxelatlas.slice/cpu.max").split() == ["200000", "100000"]

    with subtest("render and dynamic update run under the real service sandbox"):
        for job in ("voxelatlas-render", "voxelatlas-update-data"):
            machine.succeed(f"systemctl start {job}.service", timeout=60)
            assert machine.succeed(f"systemctl show {job}.service -p Result --value").strip() == "success"
            assert machine.succeed(f"systemctl show {job}.service -p ExecMainStatus --value").strip() == "0"
        machine.succeed("python3 ${verify}")

    with subtest("incremental rerender preserves the published manifest"):
        before = machine.succeed("cat /var/lib/voxelatlas/output/current.json")
        machine.succeed("systemctl start voxelatlas-render.service", timeout=60)
        assert machine.succeed("cat /var/lib/voxelatlas/output/current.json") == before
        machine.succeed("python3 ${verify}")
  '';
}
