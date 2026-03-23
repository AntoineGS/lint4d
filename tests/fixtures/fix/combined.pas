unit CombinedFix;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

const
  maxRetries = 3;

implementation

procedure MyClass.DoWork;
var
  RetryCount: Integer;
begin
  RetryCount := maxRetries;
end;

end.
