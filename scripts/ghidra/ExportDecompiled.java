// Writes Ghidra's decompiled C for every function of the current program to
// one text file. Used headless by scripts/ghidra/decompile.ps1:
//   analyzeHeadless ... -postScript ExportDecompiled.java <output.c>
//@category Export

import java.io.File;
import java.io.FileWriter;
import java.io.PrintWriter;

import ghidra.app.decompiler.DecompInterface;
import ghidra.app.decompiler.DecompileResults;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.listing.Function;
import ghidra.program.model.listing.FunctionIterator;

public class ExportDecompiled extends GhidraScript {
    @Override
    public void run() throws Exception {
        String[] args = getScriptArgs();
        if (args.length != 1) {
            printerr("usage: ExportDecompiled.java <output.c>");
            return;
        }
        DecompInterface decompiler = new DecompInterface();
        decompiler.openProgram(currentProgram);
        int ok = 0, failed = 0;
        try (PrintWriter out = new PrintWriter(new FileWriter(new File(args[0])))) {
            out.println("// " + currentProgram.getName() + " (" + currentProgram.getLanguageID() + ")");
            FunctionIterator functions = currentProgram.getFunctionManager().getFunctions(true);
            while (functions.hasNext() && !monitor.isCancelled()) {
                Function f = functions.next();
                out.println();
                out.println("// ===== " + f.getName() + " @ " + f.getEntryPoint()
                        + (f.isThunk() ? " (thunk)" : ""));
                DecompileResults r = decompiler.decompileFunction(f, 60, monitor);
                if (r != null && r.decompileCompleted()) {
                    out.println(r.getDecompiledFunction().getC());
                    ok++;
                } else {
                    out.println("// decompile failed: " + (r == null ? "no result" : r.getErrorMessage()));
                    failed++;
                }
            }
        } finally {
            decompiler.dispose();
        }
        println("Exported " + ok + " functions (" + failed + " failed) to " + args[0]);
    }
}
