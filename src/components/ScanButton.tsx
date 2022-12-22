import Loading from "./Loading";

export default function ({
  isScanning,
  toggleScan,
}: {
  isScanning: boolean;
  toggleScan: () => void;
}) {
  return (
    <button
      type="button"
      onClick={toggleScan}
      className={
        "text-lg font-bold pl-4 pr-4 m-4 border-gray-500 border-solid border-2 rounded-sm shadow-zinc-900 shadow-md hover:border-gray-300"
      }
    >
      {isScanning ? (
        <div className="flex">
          <div>Scanning</div>
          <Loading />
        </div>
      ) : (
        <div>Search for toys</div>
      )}
    </button>
  );
}