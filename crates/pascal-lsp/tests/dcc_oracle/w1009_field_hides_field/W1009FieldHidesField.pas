unit W1009FieldHidesField;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

  TChild = class(TBase)
  protected
    Value: Integer;
  end;

implementation

end.
