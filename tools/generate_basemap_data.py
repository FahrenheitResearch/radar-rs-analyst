import math
import struct
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE_DIR = ROOT / "work" / "basemap_sources"
OUT = ROOT / "crates" / "app_ui" / "src" / "basemap_data.rs"


def read_zip_member(zip_name, suffix):
    with zipfile.ZipFile(SOURCE_DIR / zip_name) as archive:
        name = next(name for name in archive.namelist() if name.endswith(suffix))
        return archive.read(name)


def read_dbf(zip_name):
    data = read_zip_member(zip_name, ".dbf")
    record_count = struct.unpack_from("<I", data, 4)[0]
    header_len = struct.unpack_from("<H", data, 8)[0]
    record_len = struct.unpack_from("<H", data, 10)[0]
    fields = []
    offset = 1
    pos = 32
    while data[pos] != 0x0D:
        raw_name = data[pos : pos + 11].split(b"\x00", 1)[0]
        name = raw_name.decode("ascii", "ignore")
        field_type = chr(data[pos + 11])
        length = data[pos + 16]
        fields.append((name, field_type, offset, length))
        offset += length
        pos += 32

    records = []
    for record_index in range(record_count):
        start = header_len + record_index * record_len
        if start + record_len > len(data) or data[start : start + 1] == b"*":
            records.append({})
            continue
        row = {}
        for name, field_type, field_offset, length in fields:
            raw = data[start + field_offset : start + field_offset + length]
            text = raw.decode("latin1", "ignore").strip()
            if field_type in ("N", "F"):
                if text == "":
                    value = 0.0
                else:
                    try:
                        value = float(text)
                    except ValueError:
                        value = 0.0
            else:
                value = text
            row[name] = value
        records.append(row)
    return records


def read_shapes(zip_name):
    data = read_zip_member(zip_name, ".shp")
    shapes = []
    pos = 100
    while pos + 8 <= len(data):
        content_words = struct.unpack_from(">i", data, pos + 4)[0]
        content_len = content_words * 2
        content_start = pos + 8
        content_end = content_start + content_len
        if content_end > len(data):
            break
        shape_type = struct.unpack_from("<i", data, content_start)[0]
        if shape_type == 0:
            shapes.append(None)
        elif shape_type == 1:
            x, y = struct.unpack_from("<2d", data, content_start + 4)
            shapes.append({"type": "point", "point": (x, y)})
        elif shape_type in (3, 5):
            xmin, ymin, xmax, ymax = struct.unpack_from("<4d", data, content_start + 4)
            num_parts, num_points = struct.unpack_from("<2i", data, content_start + 36)
            parts_offset = content_start + 44
            points_offset = parts_offset + num_parts * 4
            parts = [
                struct.unpack_from("<i", data, parts_offset + index * 4)[0]
                for index in range(num_parts)
            ]
            points = [
                struct.unpack_from("<2d", data, points_offset + index * 16)
                for index in range(num_points)
            ]
            rings = []
            for part_index, start in enumerate(parts):
                end = parts[part_index + 1] if part_index + 1 < len(parts) else num_points
                rings.append(points[start:end])
            shapes.append(
                {
                    "type": "line",
                    "bbox": (xmin, ymin, xmax, ymax),
                    "rings": rings,
                }
            )
        else:
            shapes.append(None)
        pos = content_end
    return shapes


def perpendicular_distance(point, start, end):
    x, y = point
    x1, y1 = start
    x2, y2 = end
    dx = x2 - x1
    dy = y2 - y1
    if dx == 0.0 and dy == 0.0:
        return math.hypot(x - x1, y - y1)
    return abs(dy * x - dx * y + x2 * y1 - y2 * x1) / math.hypot(dx, dy)


def simplify(points, tolerance):
    if len(points) <= 2:
        return points
    closed = points[0] == points[-1]
    work = points[:-1] if closed else points
    if len(work) <= 2:
        return points

    keep = [False] * len(work)
    keep[0] = True
    keep[-1] = True
    stack = [(0, len(work) - 1)]
    while stack:
        start, end = stack.pop()
        max_distance = -1.0
        max_index = None
        for index in range(start + 1, end):
            distance = perpendicular_distance(work[index], work[start], work[end])
            if distance > max_distance:
                max_distance = distance
                max_index = index
        if max_index is not None and max_distance > tolerance:
            keep[max_index] = True
            stack.append((start, max_index))
            stack.append((max_index, end))

    simplified = [point for point, should_keep in zip(work, keep) if should_keep]
    if closed and simplified[0] != simplified[-1]:
        simplified.append(simplified[0])
    return simplified


def bbox_for(points):
    xs = [point[0] for point in points]
    ys = [point[1] for point in points]
    return (min(xs), min(ys), max(xs), max(ys))


def ring_area(points):
    if len(points) < 3:
        return 0.0
    area = 0.0
    for left, right in zip(points, points[1:]):
        area += left[0] * right[1] - right[0] * left[1]
    return abs(area) * 0.5


def ascii_text(value):
    value = str(value).replace("\\", "").replace('"', "").strip()
    return value.encode("ascii", "ignore").decode("ascii")


def make_lines(zip_name, tolerance, min_area, name_field=None, state_field=None):
    shapes = read_shapes(zip_name)
    records = read_dbf(zip_name)
    lines = []
    labels = []
    for shape, record in zip(shapes, records):
        if not shape or shape["type"] != "line":
            continue
        largest_ring = None
        largest_area = 0.0
        for ring in shape["rings"]:
            if len(ring) < 2:
                continue
            area = ring_area(ring)
            if area < min_area:
                continue
            if area > largest_area:
                largest_area = area
                largest_ring = ring
            simplified = simplify(ring, tolerance)
            if len(simplified) < 2:
                continue
            lines.append((bbox_for(simplified), simplified))
        if name_field and largest_ring:
            bbox = bbox_for(largest_ring)
            name = ascii_text(record.get(name_field, ""))
            state = ascii_text(record.get(state_field, "")) if state_field else ""
            if name:
                labels.append(
                    {
                        "name": name,
                        "state": state,
                        "lon": (bbox[0] + bbox[2]) * 0.5,
                        "lat": (bbox[1] + bbox[3]) * 0.5,
                        "bbox": bbox,
                    }
                )
    return lines, labels


def read_places():
    shapes = read_shapes("ne_10m_populated_places.zip")
    records = read_dbf("ne_10m_populated_places.zip")
    places = []
    wanted = {"USA", "CAN", "MEX"}
    for shape, record in zip(shapes, records):
        if not shape or shape["type"] != "point":
            continue
        adm0 = record.get("ADM0_A3", "")
        sov = record.get("SOV_A3", "")
        if adm0 not in wanted and sov not in wanted:
            continue
        lon, lat = shape["point"]
        population = int(record.get("POP_MAX", 0.0))
        scalerank = int(record.get("SCALERANK", 10.0))
        if population < 1_000 and scalerank > 8:
            continue
        if population >= 1_000_000:
            rank = 0
        elif population >= 250_000:
            rank = 1
        elif population >= 100_000:
            rank = 2
        elif population >= 50_000:
            rank = 3
        elif population >= 20_000:
            rank = 4
        elif population >= 10_000:
            rank = 5
        else:
            rank = 6
        name = ascii_text(record.get("NAMEASCII") or record.get("NAME") or "")
        if not name:
            continue
        places.append(
            {
                "name": name,
                "lon": lon,
                "lat": lat,
                "population": population,
                "rank": min(rank, scalerank),
            }
        )
    places.sort(key=lambda place: (place["rank"], -place["population"], place["name"]))
    return places


def rust_float(value):
    return f"{value:.5f}"


def write_lines(handle, const_name, lines):
    handle.write("#[rustfmt::skip]\n")
    handle.write(f"pub const {const_name}: &[BasemapLine] = &[\n")
    for bbox, points in lines:
        handle.write(
            "    BasemapLine { bbox: ["
            + ", ".join(rust_float(value) for value in bbox)
            + "], points: &[\n"
        )
        for lon, lat in points:
            handle.write(f"        ({rust_float(lon)}, {rust_float(lat)}),\n")
        handle.write("    ] },\n")
    handle.write("];\n\n")


def main():
    state_lines, _ = make_lines(
        "cb_2024_us_state_500k.zip", tolerance=0.004, min_area=0.00002
    )
    county_lines, county_labels = make_lines(
        "cb_2024_us_county_500k.zip",
        tolerance=0.006,
        min_area=0.00001,
        name_field="NAME",
        state_field="STATEFP",
    )
    places = read_places()

    OUT.parent.mkdir(parents=True, exist_ok=True)
    with OUT.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write("// Generated by work/generate_basemap_data.py.\n")
        handle.write("// Source data: US Census 2024 cartographic boundaries and Natural Earth populated places.\n\n")
        handle.write("#![allow(clippy::excessive_precision)]\n\n")
        handle.write("#[derive(Clone, Copy, Debug)]\n")
        handle.write("pub struct BasemapLine {\n")
        handle.write("    pub bbox: [f32; 4],\n")
        handle.write("    pub points: &'static [(f32, f32)],\n")
        handle.write("}\n\n")
        handle.write("#[derive(Clone, Copy, Debug)]\n")
        handle.write("pub struct BasemapLabel {\n")
        handle.write("    pub name: &'static str,\n")
        handle.write("    pub lon: f32,\n")
        handle.write("    pub lat: f32,\n")
        handle.write("    pub rank: u8,\n")
        handle.write("}\n\n")
        write_lines(handle, "BASEMAP_STATE_LINES", state_lines)
        write_lines(handle, "BASEMAP_COUNTY_LINES", county_lines)
        handle.write("#[rustfmt::skip]\n")
        handle.write("pub const BASEMAP_COUNTY_LABELS: &[BasemapLabel] = &[\n")
        for label in county_labels:
            handle.write(
                f"    BasemapLabel {{ name: \"{label['name']}\", "
                f"lon: {rust_float(label['lon'])}, lat: {rust_float(label['lat'])}, rank: 4 }},\n"
            )
        handle.write("];\n\n")
        handle.write("#[rustfmt::skip]\n")
        handle.write("pub const BASEMAP_PLACE_LABELS: &[BasemapLabel] = &[\n")
        for place in places:
            handle.write(
                f"    BasemapLabel {{ name: \"{place['name']}\", "
                f"lon: {rust_float(place['lon'])}, lat: {rust_float(place['lat'])}, "
                f"rank: {int(place['rank'])} }},\n"
            )
        handle.write("];\n")

    print(f"state_lines={len(state_lines)} county_lines={len(county_lines)} county_labels={len(county_labels)} places={len(places)}")
    print(f"wrote={OUT} bytes={OUT.stat().st_size}")


if __name__ == "__main__":
    main()
