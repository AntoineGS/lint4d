unit GoodFactoryMethod;

interface

implementation

procedure TestFactory;
var
  Runner: ITestRunner;
  Logger: ILogger;
begin
  Runner := TDUnitX.CreateRunner;
  Logger := TLoggerFactory.CreateLogger;
  Runner.AddLogger(Logger);
  Runner.Execute;
end;

end.
