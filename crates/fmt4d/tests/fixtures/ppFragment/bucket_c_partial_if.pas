unit BucketCPartialIf;

interface

type
  TLogger = class
  public
    procedure LogDebug(const msg: string);
  end;

implementation

procedure TLogger.LogDebug(const msg: string);
begin
  {$ifndef DEBUG} if WsDebugLogs then {$endif}
    LogIntoWorkerFile('Debug', msg);
end;

end.
