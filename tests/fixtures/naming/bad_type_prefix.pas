unit BadTypePrefix;

interface

type
  MyClass = class(TObject)
  public
    procedure DoWork;
  end;

  BadRecord = record
    X: Integer;
  end;

implementation

procedure MyClass.DoWork;
begin
end;

end.
