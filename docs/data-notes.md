# Data notes

Format, CRS, vertical datum, tiling and known gaps of each dataset. Provenance, licenses and checksums are in [`data/sources.toml`](../data/sources.toml); attribution is in [`ATTRIBUTION.md`](../ATTRIBUTION.md).

## How the LGV data is delivered

The LGV (daten-hamburg.de) publishes rasters, CityGML and NAS only as city-wide or district-wide zip archives of up to 4.5 GB. There are no Atom feeds, no WCS and no per-tile URLs.

- The server answers HTTP `Range` requests (`accept-ranges: bytes`, 206), so `hhmc fetch` reads only the zip central directory and the wanted members (`member` / `member_prefix` in `sources.toml`). The pilot area needs about 1.8 GB out of more than 20 GB of archives.
- `www.daten-hamburg.de` returns HTTP 429 after bursts of requests. Fetch retries with exponential backoff.
- `daten-hamburg.de/...` URLs answer 301 and redirect to `www.daten-hamburg.de/...`. `sources.toml` uses the final URLs.
- The WFS datasets (trees, ALKIS vereinfacht, boundary) are fetched as GeoJSON GetFeature responses with an explicit `SORTBY`. The responses are byte-identical across repeated requests, so they can be pinned. When the publisher updates the data, fetch reports a checksum mismatch for these `volatile` sources; review the change, then run `hhmc fetch --repin --dataset <id>`.

## Height datums

All heights are NHN (DHHN2016) unless noted below. Hamburg's quasigeoid is GCG2016.

- The 3D street-tree model is stated in DHHN92. The difference to DHHN2016 in Hamburg is a few centimeters, which is irrelevant at 1 m voxels.
- LoD3.0-HH declares no vertical CRS (`srsName="EPSG:25832"`, a 2D code on 3D coordinates). Its "Grundhöhe NN" attribute and its agreement with DGM1 indicate NHN, which is assumed.

## Datasets

### dgm1: Geländemodelle Hamburg, DGM1 2022

- GeoTIFF, Float32, LZW, 1000 × 1000 px per 1 km tile, in a zip folder per E km whose case varies (`s32_563/`, `S32_…`). Files are named `dgm1_32_<Ekm>_<Nkm>_1_hh_2022.tif`.
- The grid is 1 m, pixel-is-area, with its origin on whole kilometers, so the pixels align with the tile grid (E0 and N0 are whole meters). NoData is −3.4028235e+38.
- EPSG:25832 is embedded. The vertical datum DHHN2016 comes only from the CSV metadata inside the zip (`dgm1_hh_2023-02-09.csv`).
- Airborne laser scan from 2022-03, accurate to about ±0.15 m on firm surfaces and less under bridges and dense vegetation.
- Water surfaces are flattened to a constant 2.0 m NHN, so there is no bathymetry. In tile 565_5932, 58 % of pixels are exactly 2.0.

### bdom1: Digitales Höhenmodell Hamburg bDOM 2022

Hamburg publishes no open laser surface model and no LAZ point cloud; the DGM record offers derived products only on commission. bDOM, an image-matching surface model produced as a by-product of the True-DOP, stands in for it.

- GeoTIFF, Float32, uncompressed, 1000 × 1000 px, 1 m, aligned like DGM1, with no NoData tag. Members are named `S32_<E>/dom1_32_<E>_<N>_1_hh_2022.tif`.
- Its date is given as Aktualität 2022-11, without saying whether that is the flight or the production date.
- Values over water are matching noise (−1.6 to −1.2 m sampled in the Elbe), so water columns are masked in QA. The model predates recent HafenCity buildings.

### dop20: DOP Zeitreihe unbelaubt, True-DOP 2026

- Leaf-off flight on 2026-03-19 with an UltraCam Osprey 4.1.
- RGBI 8-bit, 20 cm, one GeoTIFF of 5000 × 5000 px per 1 km tile, JPEG q90 with internal 256 px tiles and an alpha mask, no NoData.
- The georeferencing origin carries a float offset of about 5e-7 m, which prep snaps.
- There is one zip per Bezirk, and tiles on district borders are split into complementary masked parts. The `_1` … `_4` suffix of `dop20rgbi_32_<E>_<N>_1_hh_2026_<part>.tif` names the district (Mitte, Altona, Eimsbüttel and Hamburg-Nord in the inner city). Prep mosaics the parts.
- The web map loads the same imagery from the WMS `https://geodienste.hamburg.de/wms_dop_zeitreihe_unbelaubt`, layer `dop_zeitreihe_unbelaubt`. It has a `TIME` dimension from 2001 to 2026 with 2026 as the default, and offers EPSG:3857 and EPSG:25832.
- The 2023 time slice (Maxar) has a closed license and is not used.

### dgmw: DGM-W 2022 Unter- und Außenelbe, Hamburg part (WSV)

- The license is GeoNutzV, not dl-de. The metadata's source field is empty, so the attribution wording in `sources.toml` is derived from the publisher and dataset names. GeoNutzV §3 requires a change note, hence "Daten geändert".
- GeoTIFF, Float32, LZW, 128 px internal tiles, 1 m on whole meters, NoData −9999.
- The Hamburg part covers E 560000–574000, N 5919000–5934000 (14000 × 15000 px); the parts Wedel, Buxtehude and Geesthacht extend it along the river.
- Heights above water come from a laser scan in 04/2022, below water from multibeam and single-beam echo sounding 2017–2023.
- The inner city north of a diagonal at N ≈ 5933400–5933740 is NoData, as is all land and Alster water away from the Elbe; DGM1 covers these. The Norderelbe bed is about −13.4 m NHN. Several Fleet channels show beds of about −3.5 m, below mean tidal low water.
- The data-source map (`Datenquellkarte_…zip`) gives the survey method and period of each area: 3091 polygons with fields ShapeID, MD_UUID, PARENTID, MD_TITEL, DATENART, DA_UK, HERKUNFT, ANFANG, ENDE, Y_ANFANG and Y_ENDE.

### lod2: 3D-Gebäudemodell LoD2-DE Hamburg, release 2026-04-28

- CityGML 1.0, AdV profile, in 1 km tiles `LoD2_32_<E>_<N>_1_HH.gml`. Shared geometry is stored redundantly.
- Namespaces `core=http://www.opengis.net/citygml/1.0`, `bldg=…/building/1.0`, `gen=…/generics/1.0`, `app=…/appearance/1.0` and `gml=http://www.opengis.net/gml`; `srsName="urn:adv:crs:ETRS89_UTM32*DE_DHHN2016_NH"`.
- Each `bldg:Building` has a `lod2Solid` with RoofSurface, WallSurface and GroundSurface, and the attributes function, roofType, measuredHeight, storeysAboveGround and gen attributes for its data sources.
- The `gml:id` is the ALKIS id (`DEHHALKA…`), which is the key for matching LoD3 buildings.
- Footprints are from early 2026, heights from ALS 2022 plus image matching 2024/25, accurate to about ±1 m.
- The metadata record 1F6E48EB… describes a different model, the INSPIRE LoD2 last updated in 2019, which is not used.

### lod3: 3D-Gebäudemodell LoD3.0-HH, textured

- CityGML 2.0 with namespaces `core=http://www.opengis.net/citygml/2.0`, `bldg=…/building/2.0`, `app=…/appearance/2.0` and `gen=…/generics/2.0`. The header also declares brid, tran, veg, wtr, luse, dem, frn, tun, grp, tex and pbase (FME output).
- `srsName="EPSG:25832"` on 3D coordinates.
- The files contain Building, lod3Solid, lod3MultiSurface, Wall/Roof/GroundSurface, OuterCeilingSurface, OuterFloorSurface, (Outer)BuildingInstallation and lod3TerrainIntersection. They contain no BuildingPart and no Window, Door or Opening features, so windows are detected in the facade textures.
- Textures are `app:ParameterizedTexture`, theme `rgbTexture`, JPEG. The `imageURI` uses backslashes (`images\<uuid>.jpg`).
- There are five area zips per variant (textured and untextured, with identical geometry). One 1 km tile can appear in several areas with disjoint buildings, so prep merges them per tile. Folder `<EE><NN>/` is the 1 km tile E 5EE km, N 59NN km.
- The textured Area 1 and Area 3 zips use Deflate64 (zip method 9), which is why the fetcher enables the zip crate's `deflate64` feature. The zips also contain `Thumbs.db` files.
- The model is based on a nadir and oblique flight from 2020 and ALKIS 03/2020. Buildings completed after 2020, including much of HafenCity, are missing and come from LoD2.

### trees, trees_hpa, trees3d: street trees

- `trees` is the BUKEA Straßenbaumkataster (WFS `de.hh.up:strassenbaumkataster`) with the fields baumid, baumnummer, gattung*, art*, sorte*, pflanzjahr, kronendurchmesser (m), stammumfang (cm), address fields and daten_aus_vorjahr. It has no height, and a few trees have no crown diameter.
- `trees_hpa` is the HPA port tree cadaster (WFS `de.hh.up:strbaeume_hpa`) with baumnummer, kronendurchmesser, stammdurchmesser (the trunk diameter, not the girth), art (German), baumart (Latin), gruenanlage and standortfkt. It does not overlap the BUKEA cadaster, so the two sets are additive.
- `trees3d` is the LGV 3D Straßenbaumkataster (CityGML 2.0, DHHN92): `veg:SolitaryVegetationObject` with implicit geometry and gen attributes, including `Hoehe_aus_ALS` (m). Positions are as of 09/2021, heights from ALS 04/2022. It holds 224,185 trees city-wide, 222,796 with a height; crown diameter divided by ALS height has a median of 0.567.
- `trees3d` is joined to the current cadaster by tree number and accepted when the two positions are within 3 m, which covers about three quarters of the street trees. Trees planted or renumbered since 2021 get a height from genus and crown diameter (`trees/calibration.json`).
- bDOM works as a canopy height model, with a bias: at the joined street trees, the bDOM − DGM1 maximum within the crown is 2.5 m below the ALS height at the median (p10 0.6, p90 5.3), since the aerial images are leaf-off. A linear fit gives ALS ≈ 4.67 + 0.785 × CHM, r² 0.75.

### alkis_nutzung: ALKIS vereinfacht, `ave:Nutzung` (WFS)

The fields are oid, aktualit, nutzart, bez and name. The values are text, not codes, e.g. nutzart "Strassenverkehr", "Fliessgewaesser", "Hafenbecken", "Platz", "Weg", "Bahnverkehr", "Schiffsverkehr", "Sport Freizeit Und Erholungsflaeche", "Wohnbauflaeche", and bez "Fahrbahn", "Kanal", "Fluß", "Fußgängerzone", "Grünanlage". They are the input for the surface-class rules in `config/blocks.toml`.

### alkis_nas: ALKIS ausgewählte Daten (NAS), release 2026-10-15

ALKIS vereinfacht has no bridges or quay walls, so they come from this second ALKIS source.

- NAS (AAA-AS 7.1), root `AX_Bestandsdatenauszug`, GML 3.2, `srsName="urn:adv:crs:ETRS89_UTM32"` (2D).
- The archive is split into parts `HmbTG_ALKIS_261015_<n>von289.xml`. The spatial parts are 2 km × 2 km, with SW corners at odd E km and even N km (from `suedwestEcke`). Part numbers change between releases.
- Features crossing part boundaries appear in each part; prep dedupes them by `gml:id`.
- The classes used are AX_BauwerkImVerkehrsbereich (`bauwerksfunktion` 1800 Brücke, 1810 Landebrücke, 1830 Hochbahn/Hochstraße, 1840 Brückenpfeiler, 1890 Schleusenkammer), AX_BauwerkImGewaesserbereich (2136 Ufermauer/Kaimauer, mostly lines), AX_Gleis, AX_EinrichtungenFuerDenSchiffsverkehr, AX_SonstigesBauwerkOderSonstigeEinrichtung and the full land-use classes.
- Code lists resolve at `https://registry.gdi-de.org/codelist/de.adv-online.gid/<Codelist>/<code>`.

### ALKIS NAS structures

`hhmc-prep::nas` reads them. Code values follow the GeoInfoDok value lists, as parsed into norBIT's `alkisimport` (`alkis-schema.sql`):

| Class | Attribute | Values used |
| --- | --- | --- |
| AX_BauwerkImVerkehrsbereich | bauwerksfunktion | 1800 Brücke, 1810 Landebrücke, 1820 Steg, 1830 Hochbahn/Hochstraße, 1840 Brückenpfeiler, 1870 Tunnel/Unterführung (1890 Schleusenkammer, unused) |
| AX_SonstigesBauwerkOderSonstigeEinrichtung | bauwerksfunktion | 1610 Überdachung (occluder; LoD2 function `51009_1610`) |
| AX_Gebaeude | lageZurErdoberflaeche | 1200 Unter der Erdoberfläche, 1400 Aufgeständert |
| AX_Gleis | bahnkategorie | 1100 Eisenbahn (also the U3 on the viaduct), 9999 Sonstiges |

- Bridges have almost no `name`; tunnels mostly do (e.g. "U-Bahn#U3").
- `AX_EinrichtungenFuerDenSchiffsverkehr` `art` 1470 is not in that value list (newer GeoInfoDok) and is not used.
- The buildings on the Landungsbrücken pontoons are `gebaeudefunktion` 2080 (Gebäude für Bewirtung) and 2444 (Bootshaus).

### boundary: Landesgrenze (WFS `app:landesgrenze`)

One MultiPolygon with 4 parts, including Neuwerk and Scharhörn, which the full area excludes.

### osm: Geofabrik Hamburg extract

The 1 January 2026 extract; Geofabrik keeps yearly extracts permanently but deletes daily ones after a few days. Only the positions of locks, weirs, sluice gates and dams are used, as water barriers. The license is ODbL; user and uid metadata are stripped in public extracts.

### Water levels (`config/water.toml`)

- The St. Pauli gauge has PEGELONLINE UUID `d488c5cc-4de9-4631-8ce1-0db0e700b546`, number 5952050, at Elbe km 623.1, with its zero at −5.004 m NHN since 2019-11-01. PEGELONLINE's built-in characteristic values cover 2001–2010 and are outdated; the HPA's *Gewässerkundliche Information 2026* gives MThw +2.19, MTmw +0.42 and MTnw −1.64 m NHN for 2021–2025.
- The Speicherstadt Fleete and the Zollkanal are tidal and open to the Elbe.
- The Alster is held at about +3 m NHN, the Alsterfleet at about +1.6 m behind the Schaartorschleuse.
- The Nikolaifleet has the Mühlenschleuse; its held level is not published.
- The Alster and Fleet values are approximate and marked `uncertain` in `config/water.toml`.
