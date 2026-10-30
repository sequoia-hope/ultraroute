// A board opened from the user's computer: its .kicad_pcb, and the .kicad_pro and .kicad_dru
// that go with it when given, read here in the browser. Nothing is uploaded; the tab routes it.

// The rules a new KiCad 9 project starts with: its Default net class and its board minimums.
// The core reads its rules from the .kicad_pro and nothing else, so a board opened without one
// is routed with these, which the studio says, rather than with no clearance at all.
export const KICAD_DEFAULT_PRO = JSON.stringify({
  board: {
    design_settings: {
      rules: {
        min_clearance: 0.0,
        min_track_width: 0.0,
        min_via_diameter: 0.5,
        min_through_hole_diameter: 0.3,
        min_via_annular_width: 0.1,
        min_copper_edge_clearance: 0.5,
        min_hole_clearance: 0.25,
        min_hole_to_hole: 0.25,
        allow_blind_buried_vias: false,
        allow_microvias: false,
      },
    },
  },
  net_settings: {
    classes: [
      {
        name: 'Default',
        clearance: 0.2,
        track_width: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        diff_pair_width: 0.2,
        diff_pair_gap: 0.25,
        diff_pair_via_gap: 0.25,
        priority: 2147483647,
      },
    ],
  },
});

export const DEFAULT_RULES_NOTE = "KiCad's default rules (0.2 mm clearance and tracks, 0.6 mm vias with 0.3 mm holes)";

const stem = (name, ext) => name.slice(0, -ext.length);

// The board among the files, and the project and custom rules named as it is. A lone .kicad_pro
// or .kicad_dru of another name is taken too (a routed board saved as <name>-routed.kicad_pcb,
// opened again beside its project), and said so. Throws with what is wrong, in words.
export async function readBoardFiles(fileList) {
  const files = [...fileList];
  const of = (ext) => files.filter((f) => f.name.toLowerCase().endsWith(ext));
  const pcbs = of('.kicad_pcb');
  if (!pcbs.length) throw new Error('no .kicad_pcb among the files given');
  const pcbFile = pcbs[0];
  const name = stem(pcbFile.name, '.kicad_pcb');
  const notes = [];
  if (pcbs.length > 1) notes.push(`opened ${pcbFile.name}, one board at a time`);
  const partner = (ext) => {
    const all = of(ext);
    const named = all.find((f) => stem(f.name, ext) === name);
    if (named) return named;
    if (all.length === 1) {
      notes.push(`took ${all[0].name} for ${pcbFile.name}`);
      return all[0];
    }
    return null;
  };
  const proFile = partner('.kicad_pro');
  const druFile = partner('.kicad_dru');
  const [pcb, pro, dru] = await Promise.all([pcbFile.text(), proFile?.text() ?? null, druFile?.text() ?? '']);
  if (!/^\s*\(kicad_pcb\b/.test(pcb)) throw new Error(`${pcbFile.name} is not a KiCad board`);
  if (pro != null) {
    try {
      JSON.parse(pro);
    } catch (e) {
      throw new Error(`${proFile.name} is not a KiCad project: ${e.message}`);
    }
  }
  return { name, pcbName: pcbFile.name, proName: proFile?.name ?? null, druName: druFile?.name ?? null, pcb, pro, dru, notes };
}
