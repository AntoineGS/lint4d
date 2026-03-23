unit TypePrefixFix;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

var
  Obj: MyClass;

implementation

procedure MyClass.DoWork;
var
  Local: MyClass;
begin
end;

end.
