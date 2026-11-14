// Lists the newest matches of a query through zthing.dll.
//   cargo build --release -p zthing-ffi
//   dotnet run --project examples/csharp -- "ext:pdf"
using System.Runtime.InteropServices;

var query = args.Length > 0 ? args[0] : "ext:pdf";
var zt = Zthing.zthing_connect();
try
{
    Zthing.zthing_set_query_w(zt, query);
    Zthing.zthing_set_sort(zt, Zthing.SortModified, 1);
    Zthing.zthing_set_range(zt, 0, 10);
    if (Zthing.zthing_execute(zt) != Zthing.Ok)
    {
        Console.Error.WriteLine($"search failed: {Marshal.PtrToStringUni(Zthing.zthing_last_error_w(zt))}");
        return 2;
    }
    Console.WriteLine($"{Zthing.zthing_total(zt)} matches for {query}, the newest:");
    for (ulong i = 0; i < Zthing.zthing_result_count(zt); i++)
    {
        var modified = DateTimeOffset.FromUnixTimeSeconds(Zthing.zthing_result_modified(zt, i)).ToLocalTime();
        var path = Marshal.PtrToStringUni(Zthing.zthing_result_path_w(zt, i));
        Console.WriteLine($"{modified:yyyy-MM-dd HH:mm}  {path}");
    }
    return 0;
}
finally
{
    Zthing.zthing_free(zt);
}

/// The functions of zthing.h this example needs. Returned strings belong to the
/// handle, so they are copied with Marshal.PtrToStringUni and never freed here.
static class Zthing
{
    public const int Ok = 0;
    public const int SortModified = 3;

    [DllImport("zthing")] public static extern IntPtr zthing_connect();
    [DllImport("zthing")] public static extern void zthing_free(IntPtr handle);
    [DllImport("zthing", CharSet = CharSet.Unicode)] public static extern int zthing_set_query_w(IntPtr handle, string query);
    [DllImport("zthing")] public static extern int zthing_set_sort(IntPtr handle, int sort, int descending);
    [DllImport("zthing")] public static extern int zthing_set_range(IntPtr handle, ulong offset, ulong limit);
    [DllImport("zthing")] public static extern int zthing_execute(IntPtr handle);
    [DllImport("zthing")] public static extern ulong zthing_total(IntPtr handle);
    [DllImport("zthing")] public static extern ulong zthing_result_count(IntPtr handle);
    [DllImport("zthing")] public static extern IntPtr zthing_result_path_w(IntPtr handle, ulong index);
    [DllImport("zthing")] public static extern long zthing_result_modified(IntPtr handle, ulong index);
    [DllImport("zthing")] public static extern IntPtr zthing_last_error_w(IntPtr handle);
}
